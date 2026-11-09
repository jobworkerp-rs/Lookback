use std::fs;
use std::path::{Path, PathBuf};

use coding_agent_executor::{
    CaptureGuarantee, CaptureRequest, CreatePullRequestRequest, ExecutorErrorCode,
    PROTOCOL_VERSION, PrepareRequest, PullRequestRequest, PushRequest, RepositoryConfig,
    RepositoryId, ResolveBaseRequest, ResolveRequest, SealRequest, TrustedConfig,
};
use lookback_tauri_lib::coding_agent::{NativeExecutorClient, NativeExecutorError};
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const RUN_ID: &str = "test-run";
const ATTEMPT_ID: &str = "attempt-1";
const BASE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const APPROVAL_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SCOPE_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const INTENT_DIGEST: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const CORE_NATIVE_V2_FEATURES: &[&str] = &[
    "prepare",
    "capture",
    "seal",
    "resolve",
    "artifact_only_publication",
];

#[path = "fixtures/native_interactive_rpc.rs"]
mod native_interactive_rpc;

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires LOOKBACK_JOBWORKERP_BIN pointing to a built all-in-one"]
async fn real_preparation_workflow_invokes_the_helper_through_command_runner() {
    use lookback_tauri_lib::{data::paths::DataPaths, jobworkerp::JobworkerpHandle};
    use std::{process::Stdio, time::Duration};
    let fixture = FakeExecutor::new(valid_capabilities(), helper_error("invalid_request"));
    let root = tempfile::tempdir().unwrap();
    let data = DataPaths::with_root(root.path());
    data.ensure().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = tokio::process::Command::new(std::env::var("LOOKBACK_JOBWORKERP_BIN").unwrap())
        .current_dir(root.path())
        .env("STORAGE_TYPE", "Standalone")
        .env("STORAGE_RESTORE_AT_STARTUP", "false")
        .env("GRPC_ADDR", format!("127.0.0.1:{port}"))
        .env("SQLITE_URL", data.sqlite_url())
        .env("MCP_ENABLED", "false")
        .env("WORKER_DEFAULT_CONCURRENCY", "2")
        .env(
            "WORKER_CHANNELS",
            "default,native-interactive,coding-agent-one-shot",
        )
        .env("WORKER_CHANNEL_CONCURRENCIES", "2,2,2")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let handle = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "standalone exited before readiness"
            );
            if let Ok(handle) = JobworkerpHandle::connect(&url).await {
                break handle;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let client = fixture
        .connect()
        .await
        .unwrap()
        .with_preparation_workflow(handle.clone());
    let probe_count = fixture.invocation_count();
    let result = client
        .prepare(prepare_request(fixture.repository.clone()))
        .await;
    assert_eq!(
        result,
        Err(NativeExecutorError::ExecutorRejected(
            ExecutorErrorCode::InvalidRequest
        ))
    );
    assert_eq!(fixture.invocation_count(), probe_count + 1);
    let config: TrustedConfig =
        serde_json::from_slice(&fs::read(&fixture.config).unwrap()).unwrap();
    let mut request = prepare_request(fixture.repository.clone());
    request.issue_id = Some("42".into());
    let response = json!({"protocol_version":PROTOCOL_VERSION, "status":"ok", "result":{"prepared":{
        "run_id":RUN_ID, "attempt_id":ATTEMPT_ID, "repository":fixture.repository,
        "base_branch":"main", "base_commit":BASE_COMMIT, "branch":request.expected_branch(),
        "workspace_path":config.workspace_root.join(RUN_ID).join(ATTEMPT_ID),
    }}});
    let script = fs::read_to_string(&fixture.executable).unwrap().replace(
        &shell_quote(&helper_error("invalid_request").to_string()),
        &shell_quote(&response.to_string()),
    );
    fs::write(&fixture.executable, script).unwrap();
    let client = fixture
        .connect()
        .await
        .unwrap()
        .with_preparation_workflow(handle.clone());
    let probe_count = fixture.invocation_count();
    assert_eq!(
        client.prepare(request.clone()).await.unwrap().branch,
        request.expected_branch()
    );
    assert_eq!(fixture.invocation_count(), probe_count + 1);
    handle.register_workers_from_yaml_str(&json!({
        "defaults":{"response_type":"DIRECT", "broadcast_results":false},
        "workers":[
            {"name":"native-channel-command-fixture", "runner":"COMMAND", "channel":"native-interactive"},
            {"name":"auto-channel-command-fixture", "runner":"COMMAND", "channel":"coding-agent-one-shot"},
        ],
    }).to_string(), root.path()).await.unwrap();
    for worker in [
        "native-channel-command-fixture",
        "auto-channel-command-fixture",
    ] {
        let result = handle
            .dispatch_stream_with_timeout(
                worker,
                json!({"command":"/usr/bin/printf", "args":["%s", "channel-consumed"]}),
                10,
                Some("run"),
            )
            .await
            .unwrap();
        let mut ended = false;
        tokio::time::timeout(
            Duration::from_secs(15),
            result.drain_confirmed_bytes(|chunk| {
                ended |= matches!(
                    chunk,
                    lookback_tauri_lib::jobworkerp::ListenChunk::Final { .. }
                );
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(ended, "named channel did not consume its job");
    }
    verify_compiled_source_inspection_workflow(&fixture, &handle, root.path()).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

#[cfg(unix)]
async fn verify_compiled_source_inspection_workflow(
    fixture: &FakeExecutor,
    handle: &lookback_tauri_lib::jobworkerp::JobworkerpHandle,
    root: &Path,
) {
    use std::{os::unix::fs::PermissionsExt, process::Command};
    let git = |cwd: &Path, args: &[&str]| {
        let output = Command::new("/usr/bin/git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "fixture Git failed");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let source = root.join("source");
    let remote = root.join("remote.git");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "--bare", "--initial-branch=main"]);
    git(&source, &["init", "--initial-branch=main"]);
    git(&source, &["config", "user.name", "Source Workflow Fixture"]);
    git(
        &source,
        &["config", "user.email", "fixture@example.invalid"],
    );
    fs::write(source.join("base.txt"), "base\n").unwrap();
    git(&source, &["add", "base.txt"]);
    git(&source, &["commit", "-m", "base"]);
    let base = git(&source, &["rev-parse", "HEAD"]);
    git(&source, &["push", remote.to_str().unwrap(), "main"]);
    let mut config: TrustedConfig =
        serde_json::from_slice(&fs::read(&fixture.config).unwrap()).unwrap();
    config.repositories[0].clone_url = url::Url::from_file_path(&remote).unwrap().to_string();
    fs::write(&fixture.config, serde_json::to_vec(&config).unwrap()).unwrap();
    let binary = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/debug/coding-agent-executor");
    assert!(
        binary.is_file(),
        "build coding-agent-executor before the real workflow test"
    );
    let executable = fixture.executable.with_file_name("compiled-executor");
    fs::copy(binary, &executable).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let client = NativeExecutorClient::connect(
        executable.clone(),
        fixture.config.clone(),
        sha256_file(&executable),
    )
    .await
    .unwrap()
    .with_preparation_workflow(handle.clone());
    let mut request = prepare_request(fixture.repository.clone());
    request.issue_id = Some("42".into());
    request.base_commit = base;
    let prepared = client.prepare(request.clone()).await.unwrap();
    let content = vec![255u8; 300 * 1024];
    fs::write(
        Path::new(&prepared.workspace_path).join("medium.bin"),
        &content,
    )
    .unwrap();
    let inspection = client.inspect_source(request.clone()).await.unwrap();
    assert_eq!(inspection.new_files.len(), 1);
    assert_eq!(inspection.new_files[0].content, content);
    config.max_import_bytes = 1024;
    fs::write(&fixture.config, serde_json::to_vec(&config).unwrap()).unwrap();
    let limited = NativeExecutorClient::connect(
        executable.clone(),
        fixture.config.clone(),
        sha256_file(&executable),
    )
    .await
    .unwrap()
    .with_preparation_workflow(handle.clone());
    assert_eq!(
        limited.inspect_source(request).await.unwrap_err(),
        NativeExecutorError::ExecutorRejected(ExecutorErrorCode::ResponseTooLarge)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn preparation_workflow_confirms_real_end_and_never_falls_back_to_direct_helper() {
    use jobworkerp_client::{
        jobworkerp::data::{
            MethodProtoMap, MethodSchema, ResultOutputItem, StreamingOutputType, Trailer,
            result_output_item::Item,
        },
        proto::JobworkerpProto,
    };
    use lookback_tauri_lib::jobworkerp::JobworkerpHandle;
    use native_interactive_rpc::{RpcFixture, sandbox_runner_data, start_server};
    use std::collections::HashMap;
    let mut runner = sandbox_runner_data();
    runner.name = "WORKFLOW".into();
    runner.runner_type = jobworkerp_client::jobworkerp::data::RunnerType::Workflow as i32;
    runner.runner_settings_proto =
        include_str!("fixtures/jobworkerp-proto/workflow_runner_settings.proto").into();
    runner.method_proto_map = Some(MethodProtoMap {
        schemas: HashMap::from([(
            "run".into(),
            MethodSchema {
                args_proto: include_str!("fixtures/jobworkerp-proto/workflow_run_args.proto")
                    .into(),
                result_proto: include_str!("fixtures/jobworkerp-proto/workflow_result.proto")
                    .into(),
                description: None,
                output_type: StreamingOutputType::Both as i32,
                require_client_stream: false,
                client_stream_data_proto: None,
            },
        )]),
    });
    let rpc = RpcFixture::with_runner_data(runner.clone());
    rpc.allow_workflow_registration();
    let server = start_server(rpc.clone()).await;
    let fixture = FakeExecutor::new(
        capabilities_with(&["prepare"]),
        helper_error("invalid_request"),
    );
    let client = fixture
        .connect()
        .await
        .unwrap()
        .with_preparation_workflow(JobworkerpHandle::connect(&server.url).await.unwrap());
    let probe_count = fixture.invocation_count();
    let config: TrustedConfig =
        serde_json::from_slice(&fs::read(&fixture.config).unwrap()).unwrap();
    let mut request = prepare_request(fixture.repository.clone());
    request.issue_id = Some("42".into());
    let response = json!({"protocol_version":PROTOCOL_VERSION,"status":"ok","result":{"prepared":{
        "run_id":RUN_ID,"attempt_id":ATTEMPT_ID,"repository":fixture.repository,
        "base_branch":"main","base_commit":BASE_COMMIT,"branch":request.expected_branch(),
        "workspace_path":config.workspace_root.join(RUN_ID).join(ATTEMPT_ID),
    }}});
    let descriptor = JobworkerpProto::parse_result_schema_descriptor(&runner, Some("run"))
        .unwrap()
        .unwrap();
    let encode = |status: &str, output: String| {
        assert_eq!(descriptor.full_name(), "jobworkerp.runner.WorkflowResult");
        JobworkerpProto::json_value_to_message(
            descriptor.clone(),
            &json!({"id":"actual-workflow-id","status":status,"output":output}),
            true,
            false,
        )
        .unwrap()
    };
    let data = ResultOutputItem {
        item: Some(Item::Data(encode(
            "Completed",
            serde_json::to_string(&(response.to_string() + "\n")).unwrap(),
        ))),
    };
    let end = ResultOutputItem {
        item: Some(Item::End(Trailer::default())),
    };
    rpc.set_job_result_items(vec![data.clone(), end.clone()]);
    let prepared = client.prepare(request.clone()).await;
    assert!(
        prepared.is_ok(),
        "result: {prepared:?}, RPC counts: {:?}",
        rpc.counts()
    );
    let prepared = prepared.unwrap();
    assert_eq!(prepared.branch, request.expected_branch());
    assert_eq!(fixture.invocation_count(), probe_count);
    assert_eq!(rpc.counts().enqueue_for_stream_calls, 1);
    rpc.set_job_result_items(vec![
        ResultOutputItem {
            item: Some(Item::FinalCollected(encode(
                "Completed",
                response.to_string(),
            ))),
        },
        end.clone(),
    ]);
    assert_eq!(
        client.prepare(request.clone()).await.unwrap().branch,
        request.expected_branch()
    );
    rpc.set_job_result_items(vec![data.clone(), end.clone()]);
    rpc.fail_stream_after_output();
    assert!(matches!(
        client.prepare(request.clone()).await,
        Err(NativeExecutorError::OutcomeUnknown { .. })
    ));
    assert_eq!(fixture.invocation_count(), probe_count);
    let worker = rpc.first_worker_data();
    let settings_descriptor = JobworkerpProto::parse_runner_settings_schema_descriptor(&runner)
        .unwrap()
        .unwrap();
    let settings = prost_reflect::DynamicMessage::decode(
        settings_descriptor,
        worker.runner_settings.as_slice(),
    )
    .unwrap();
    let settings: serde_json::Value = serde_json::to_value(settings).unwrap();
    let workflow: serde_json::Value =
        serde_json::from_str(settings["workflowData"].as_str().unwrap()).unwrap();
    assert_eq!(
        workflow["do"][0]["invokeExecutor"]["run"]["function"]["arguments"]["command"],
        fixture.executable.to_str().unwrap()
    );
    for items in [
        vec![data.clone()],
        vec![
            ResultOutputItem {
                item: Some(Item::Data(encode("Faulted", response.to_string()))),
            },
            end.clone(),
        ],
        vec![
            data.clone(),
            ResultOutputItem {
                item: Some(Item::FinalCollected(vec![0xff])),
            },
        ],
        vec![ResultOutputItem {
            item: Some(Item::FinalCollected(encode(
                "Completed",
                serde_json::to_string(&(response.to_string() + "\n")).unwrap(),
            ))),
        }],
        vec![data.clone(), end.clone(), data],
        vec![end.clone(), end],
    ] {
        rpc.set_job_result_items(items);
        assert!(matches!(
            client.prepare(request.clone()).await,
            Err(NativeExecutorError::OutcomeUnknown { .. })
        ));
        assert_eq!(fixture.invocation_count(), probe_count);
    }
}

#[cfg(unix)]
struct FakeExecutor {
    _temp: TempDir,
    executable: PathBuf,
    config: PathBuf,
    invocations: PathBuf,
    repository: RepositoryId,
}

#[cfg(unix)]
impl FakeExecutor {
    fn new(capabilities: serde_json::Value, response: serde_json::Value) -> Self {
        Self::new_with_capability_document(&capabilities.to_string(), response)
    }

    fn new_with_capability_document(
        capability_document: &str,
        response: serde_json::Value,
    ) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let package = root.join("package");
        fs::create_dir(&package).unwrap();
        fs::set_permissions(&package, fs::Permissions::from_mode(0o700)).unwrap();

        let executable = package.join("executor-fixture");
        let invocations = root.join("invocations.log");
        let script = format!(
            "#!/bin/sh\nprintf 'called\\n' >> {}\nif [ \"$1\" = \"--capabilities\" ]; then\n  printf '%s\\n' {}\nelse\n  cat >/dev/null\n  printf '%s\\n' {}\n  printf 'fixture-secret-must-not-escape' >&2\nfi\n",
            shell_quote(invocations.to_str().unwrap()),
            shell_quote(capability_document),
            shell_quote(&response.to_string()),
        );
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

        let (config, repository) = write_config(root, None);
        Self {
            _temp: temp,
            executable,
            config,
            invocations,
            repository,
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

    fn invocation_count(&self) -> usize {
        fs::read_to_string(&self.invocations)
            .map(|contents| contents.lines().count())
            .unwrap_or(0)
    }
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_a_wrong_package_pin_before_starting_the_executable() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = FakeExecutor::new(valid_capabilities(), helper_error("invalid_request"));
    fs::set_permissions(&fixture.executable, fs::Permissions::from_mode(0o700)).unwrap();
    let result = NativeExecutorClient::connect(
        fixture.executable.clone(),
        fixture.config.clone(),
        "0".repeat(64),
    )
    .await;

    assert!(matches!(result, Err(NativeExecutorError::InvalidPackage)));
    assert_eq!(fixture.invocation_count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_a_capability_document_that_does_not_offer_protocol_two() {
    let mut capabilities = valid_capabilities();
    capabilities["protocol_version"] = json!(1);
    let fixture = FakeExecutor::new(capabilities, helper_error("invalid_request"));

    let result = fixture.connect().await;

    assert!(matches!(
        result,
        Err(NativeExecutorError::IncompatibleProtocol)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_each_missing_core_native_protocol_two_capability() {
    for missing_feature in ["prepare"] {
        let mut capabilities = valid_capabilities();
        capabilities["features"]
            .as_array_mut()
            .unwrap()
            .retain(|feature| feature.as_str() != Some(missing_feature));
        let fixture = FakeExecutor::new(capabilities, helper_error("invalid_request"));

        assert!(
            matches!(
                fixture.connect().await,
                Err(NativeExecutorError::IncompatibleProtocol)
            ),
            "the native v2 helper must advertise {missing_feature}"
        );
        assert_eq!(
            fixture.invocation_count(),
            1,
            "only capabilities are probed"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn source_inspection_requires_capability_and_matches_the_approved_workspace() {
    let unsupported = FakeExecutor::new(valid_capabilities(), helper_error("invalid_request"));
    let client = unsupported.connect().await.unwrap();
    assert_eq!(
        client
            .inspect_source(prepare_request(unsupported.repository.clone()))
            .await
            .unwrap_err(),
        NativeExecutorError::UnsupportedCapability {
            capability: "inspect_source"
        }
    );
    assert_eq!(unsupported.invocation_count(), 1);
    for mismatched in [false, true] {
        let mut capabilities = valid_capabilities();
        capabilities["features"]
            .as_array_mut()
            .unwrap()
            .push(json!("inspect_source"));
        let response = json!({"protocol_version":PROTOCOL_VERSION,"status":"ok","result":{"source_inspected":{
            "run_id":RUN_ID,"attempt_id":if mismatched {"other-attempt"} else {ATTEMPT_ID},
            "base_commit":BASE_COMMIT,"head_commit":BASE_COMMIT,"branch":format!("agent/{RUN_ID}"),
            "tracked_diff":[],"new_files":[{"path":"new.txt","is_symlink":false,"content":[65,10]}],
        }}});
        let fixture = FakeExecutor::new(capabilities, response);
        let client = fixture.connect().await.unwrap();
        let result = client
            .inspect_source(prepare_request(fixture.repository.clone()))
            .await;
        if mismatched {
            assert!(matches!(
                result,
                Err(NativeExecutorError::OutcomeUnknown {
                    operation: "inspect_source"
                })
            ));
        } else {
            assert_eq!(result.unwrap().new_files[0].content, b"A\n");
        }
        assert_eq!(fixture.invocation_count(), 2);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn source_inspection_receives_medium_binary_files_over_the_json_transport() {
    use lookback_tauri_lib::jobworkerp::JobworkerpHandle;
    use native_interactive_rpc::{RpcFixture, start_server};
    let mut capabilities = valid_capabilities();
    capabilities["features"]
        .as_array_mut()
        .unwrap()
        .push(json!("inspect_source"));
    let content = vec![255u8; 300 * 1024];
    let response = json!({"protocol_version":PROTOCOL_VERSION,"status":"ok","result":{"source_inspected":{
        "run_id":RUN_ID,"attempt_id":ATTEMPT_ID,"base_commit":BASE_COMMIT,"head_commit":BASE_COMMIT,
        "branch":format!("agent/{RUN_ID}"),"tracked_diff":[],
        "new_files":[{"path":"medium.bin","is_symlink":false,"content":content}],
    }}});
    assert!(response.to_string().len() > 1024 * 1024);
    let fixture = FakeExecutor::new(capabilities, response);
    let client = fixture.connect().await.unwrap();
    let inspection = client
        .inspect_source(prepare_request(fixture.repository.clone()))
        .await
        .unwrap();
    assert_eq!(inspection.new_files[0].content, content);
    let rpc = RpcFixture::default();
    rpc.enable_preparation_workflow();
    let server = start_server(rpc.clone()).await;
    let client =
        client.with_preparation_workflow(JobworkerpHandle::connect(&server.url).await.unwrap());
    let inspection = client
        .inspect_source(prepare_request(fixture.repository.clone()))
        .await
        .unwrap();
    assert_eq!(inspection.new_files[0].content, content);
    assert_eq!(rpc.counts().enqueue_for_stream_calls, 1);
    assert_eq!(fixture.invocation_count(), 3);
}

#[cfg(unix)]
#[tokio::test]
async fn accepts_prepare_only_and_returns_pre_dispatch_unsupported_capabilities() {
    let fixture = FakeExecutor::new(
        capabilities_with(&["prepare"]),
        helper_error("invalid_request"),
    );
    let client = fixture.connect().await.unwrap();
    assert_eq!(
        fixture.invocation_count(),
        1,
        "connect only probes capabilities"
    );

    for (capability, error) in [
        (
            "capture",
            client
                .capture(CaptureRequest {
                    run_id: RUN_ID.into(),
                    attempt_id: ATTEMPT_ID.into(),
                    repository: fixture.repository.clone(),
                    base_branch: "main".into(),
                    base_commit: BASE_COMMIT.into(),
                    approved_snapshot_digest: APPROVAL_DIGEST.into(),
                    scope_digest: SCOPE_DIGEST.into(),
                    intent_approval_digest: INTENT_DIGEST.into(),
                })
                .await
                .unwrap_err(),
        ),
        (
            "seal",
            client
                .seal(SealRequest {
                    run_id: RUN_ID.into(),
                    attempt_id: ATTEMPT_ID.into(),
                    manifest_digest: APPROVAL_DIGEST.into(),
                })
                .await
                .unwrap_err(),
        ),
        (
            "resolve",
            client
                .resolve(ResolveRequest {
                    run_id: RUN_ID.into(),
                    attempt_id: ATTEMPT_ID.into(),
                    artifact_digest: APPROVAL_DIGEST.into(),
                })
                .await
                .unwrap_err(),
        ),
    ] {
        assert_eq!(
            error,
            NativeExecutorError::UnsupportedCapability { capability }
        );
    }
    assert_eq!(
        client
            .resolve_base(resolve_base_request(fixture.repository.clone()))
            .await
            .unwrap_err(),
        NativeExecutorError::IncompatibleProtocol,
        "resolve_base remains independently optional"
    );
    assert_eq!(
        client
            .push(push_request(fixture.repository.clone()))
            .await
            .unwrap_err(),
        NativeExecutorError::UnsupportedCapability { capability: "push" }
    );
    assert_eq!(
        client
            .create_pull_request(pull_request(fixture.repository.clone()))
            .await
            .unwrap_err(),
        NativeExecutorError::UnsupportedCapability {
            capability: "pull_request"
        }
    );
    assert_eq!(
        fixture.invocation_count(),
        1,
        "unsupported publication must not dispatch to the helper"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn accepts_legacy_eight_and_current_nine_capability_sets() {
    let legacy = FakeExecutor::new(valid_capabilities(), helper_error("invalid_request"));
    let legacy_client = legacy.connect().await.unwrap();
    assert_eq!(legacy.invocation_count(), 1);
    assert_eq!(
        legacy_client
            .resolve_base(resolve_base_request(legacy.repository.clone()))
            .await
            .unwrap_err(),
        NativeExecutorError::IncompatibleProtocol
    );
    assert_eq!(legacy.invocation_count(), 1);

    let current = FakeExecutor::new(current_capabilities(), helper_error("invalid_request"));
    let current_client = current.connect().await.unwrap();
    assert!(matches!(
        current_client
            .resolve_base(resolve_base_request(current.repository.clone()))
            .await,
        Err(NativeExecutorError::ExecutorRejected(
            ExecutorErrorCode::InvalidRequest
        ))
    ));
    assert_eq!(current.invocation_count(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_capability_dependency_violations_unknown_and_duplicate_features() {
    let invalid_sets = [
        [CORE_NATIVE_V2_FEATURES, &["push"][..]].concat(),
        [
            CORE_NATIVE_V2_FEATURES,
            &["secret_scan_supported_formats_v1"][..],
        ]
        .concat(),
        [CORE_NATIVE_V2_FEATURES, &["pull_request"][..]].concat(),
        [
            CORE_NATIVE_V2_FEATURES,
            &["unsupported_future_operation"][..],
        ]
        .concat(),
        [CORE_NATIVE_V2_FEATURES, &["prepare"][..]].concat(),
    ];
    for features in invalid_sets {
        let fixture = FakeExecutor::new(
            capabilities_with(&features),
            helper_error("invalid_request"),
        );
        assert!(matches!(
            fixture.connect().await,
            Err(NativeExecutorError::IncompatibleProtocol)
        ));
        assert_eq!(fixture.invocation_count(), 1);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_publication_capabilities_when_a_required_dependency_is_absent() {
    let push_without_scan = [CORE_NATIVE_V2_FEATURES, &["push"][..]].concat();
    let fixture = FakeExecutor::new(
        capabilities_with(&push_without_scan),
        helper_error("invalid_request"),
    );
    assert!(matches!(
        fixture.connect().await,
        Err(NativeExecutorError::IncompatibleProtocol)
    ));
    assert_eq!(fixture.invocation_count(), 1);

    let scan_without_push = [
        CORE_NATIVE_V2_FEATURES,
        &["secret_scan_supported_formats_v1"][..],
    ]
    .concat();
    let fixture = FakeExecutor::new(
        capabilities_with(&scan_without_push),
        helper_error("invalid_request"),
    );
    assert!(matches!(
        fixture.connect().await,
        Err(NativeExecutorError::IncompatibleProtocol)
    ));
    assert_eq!(fixture.invocation_count(), 1);

    let push_without_pr = [
        CORE_NATIVE_V2_FEATURES,
        &["push", "secret_scan_supported_formats_v1"][..],
    ]
    .concat();
    let fixture = FakeExecutor::new(
        capabilities_with(&push_without_pr),
        helper_error("invalid_request"),
    );
    let client = fixture.connect().await.unwrap();
    assert_eq!(
        client
            .create_pull_request(pull_request(fixture.repository.clone()))
            .await
            .unwrap_err(),
        NativeExecutorError::UnsupportedCapability {
            capability: "pull_request"
        }
    );
    assert_eq!(
        fixture.invocation_count(),
        1,
        "PR was rejected before helper dispatch"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_unknown_or_duplicate_capability_document_fields() {
    let serialized_features = serde_json::to_string(CORE_NATIVE_V2_FEATURES).unwrap();
    let unknown = format!(
        "{{\"protocol_version\":{PROTOCOL_VERSION},\"executor_version\":\"{}\",\"features\":{serialized_features},\"future_field\":true}}",
        coding_agent_executor::EXECUTOR_VERSION,
    );
    let duplicate = format!(
        "{{\"protocol_version\":{PROTOCOL_VERSION},\"protocol_version\":{PROTOCOL_VERSION},\"executor_version\":\"{}\",\"features\":{serialized_features}}}",
        coding_agent_executor::EXECUTOR_VERSION,
    );
    for document in [unknown, duplicate] {
        let fixture =
            FakeExecutor::new_with_capability_document(&document, helper_error("invalid_request"));
        assert!(matches!(
            fixture.connect().await,
            Err(NativeExecutorError::IncompatibleProtocol)
        ));
        assert_eq!(fixture.invocation_count(), 1);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn publication_capability_gate_revalidates_config_before_returning_unsupported() {
    let fixture = FakeExecutor::new(core_capabilities(), helper_error("invalid_request"));
    let client = fixture.connect().await.unwrap();
    let mut config = fs::read(&fixture.config).unwrap();
    config.push(b' ');
    fs::write(&fixture.config, config).unwrap();

    let result = client.push(push_request(fixture.repository.clone())).await;

    assert_eq!(
        result.unwrap_err(),
        NativeExecutorError::InvalidTrustedConfiguration,
        "the capability gate must not bypass the existing config digest/path check"
    );
    assert_eq!(fixture.invocation_count(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn validates_host_config_permissions_before_probing_the_helper() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = FakeExecutor::new(valid_capabilities(), helper_error("invalid_request"));
    fs::set_permissions(&fixture.config, fs::Permissions::from_mode(0o644)).unwrap();

    let result = fixture.connect().await;

    assert!(matches!(
        result,
        Err(NativeExecutorError::InvalidTrustedConfiguration)
    ));
    assert_eq!(fixture.invocation_count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn reports_helper_rejections_by_shared_error_code_without_exposing_stderr() {
    let fixture = FakeExecutor::new(valid_capabilities(), helper_error("invalid_request"));
    let client = fixture.connect().await.unwrap();

    let result = client
        .prepare(prepare_request(fixture.repository.clone()))
        .await;
    let error = result.unwrap_err();

    assert!(matches!(
        error,
        NativeExecutorError::ExecutorRejected(ExecutorErrorCode::InvalidRequest)
    ));
    assert!(!error.to_string().contains("fixture-secret"));
    assert!(!format!("{error:?}").contains("fixture-secret"));
}

#[cfg(unix)]
#[tokio::test]
async fn pull_request_uses_the_shared_protocol_and_preserves_the_native_rejection() {
    let fixture = FakeExecutor::new(
        valid_capabilities(),
        helper_error("pull_request_not_approved"),
    );
    let client = fixture.connect().await.unwrap();
    let request = CreatePullRequestRequest {
        run_id: RUN_ID.to_owned(),
        attempt_id: ATTEMPT_ID.to_owned(),
        repository: fixture.repository.clone(),
        branch: format!("agent/{RUN_ID}"),
        base_branch: "main".to_owned(),
        base_commit: BASE_COMMIT.to_owned(),
        artifact_digest: APPROVAL_DIGEST.to_owned(),
        frozen_commit: BASE_COMMIT.to_owned(),
        pull_request: PullRequestRequest {
            title: "Native executor test".to_owned(),
            body: "Local test only".to_owned(),
        },
    };

    let result = client.create_pull_request(request).await;

    assert!(matches!(
        result,
        Err(NativeExecutorError::ExecutorRejected(
            ExecutorErrorCode::PullRequestNotApproved
        ))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn publication_transport_and_receipt_failures_remain_unknown_even_when_not_retryable() {
    for code in ["forge_unavailable", "io_failure", "response_too_large"] {
        let fixture = FakeExecutor::new(valid_capabilities(), helper_error(code));
        let client = fixture.connect().await.unwrap();
        let result = client.push(push_request(fixture.repository.clone())).await;
        assert!(matches!(
            result,
            Err(NativeExecutorError::OutcomeUnknown { operation: "push" })
        ));

        let fixture = FakeExecutor::new(valid_capabilities(), helper_error(code));
        let client = fixture.connect().await.unwrap();
        let request = pull_request(fixture.repository.clone());
        let result = client.create_pull_request(request).await;
        assert!(matches!(
            result,
            Err(NativeExecutorError::OutcomeUnknown {
                operation: "create_pull_request"
            })
        ));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_mismatched_receipts_and_invalid_ids_without_reinvoking_the_helper() {
    let wrong_receipt = json!({
        "protocol_version": PROTOCOL_VERSION,
        "status": "ok",
        "result": {
            "prepared": {
                "run_id": "another-run",
                "attempt_id": ATTEMPT_ID,
                "repository": {"host": "example.test", "owner": "owner", "name": "repo"},
                "base_branch": "main",
                "base_commit": BASE_COMMIT,
                "branch": format!("agent/{RUN_ID}"),
                "workspace_path": "/tmp/workspace/test-run/attempt-1"
            }
        }
    });
    let fixture = FakeExecutor::new(valid_capabilities(), wrong_receipt);
    let client = fixture.connect().await.unwrap();
    let count_after_probe = fixture.invocation_count();

    let result = client
        .prepare(prepare_request(fixture.repository.clone()))
        .await;
    assert!(matches!(
        result,
        Err(NativeExecutorError::OutcomeUnknown {
            operation: "prepare"
        })
    ));
    assert_eq!(fixture.invocation_count(), count_after_probe + 1);

    let result = client
        .prepare(PrepareRequest {
            issue_id: None,
            run_id: "../not-a-run".to_owned(),
            ..prepare_request(fixture.repository.clone())
        })
        .await;
    assert!(matches!(result, Err(NativeExecutorError::InvalidRequest)));
    assert_eq!(fixture.invocation_count(), count_after_probe + 1);
}

#[cfg(unix)]
#[tokio::test]
async fn seal_receipt_must_contain_valid_commit_and_tree_ids() {
    let response = json!({
        "protocol_version": PROTOCOL_VERSION,
        "status": "ok",
        "result": {
            "sealed": {
                "run_id": RUN_ID,
                "attempt_id": ATTEMPT_ID,
                "repository": {"host": "example.test", "owner": "owner", "name": "repo"},
                "base_branch": "main",
                "base_commit": BASE_COMMIT,
                "approved_snapshot_digest": APPROVAL_DIGEST,
                "scope_digest": SCOPE_DIGEST,
                "intent_approval_digest": INTENT_DIGEST,
                "manifest_digest": APPROVAL_DIGEST,
                "finalized_bytes_digest": APPROVAL_DIGEST,
                "tree": "deadbeef",
                "commit": BASE_COMMIT,
                "artifact_digest": APPROVAL_DIGEST,
                "protocol_version": PROTOCOL_VERSION,
                "executor_version": coding_agent_executor::EXECUTOR_VERSION,
                "capture_guarantee": "host_isolated_after_seal"
            }
        }
    });
    let fixture = FakeExecutor::new(valid_capabilities(), response);
    let client = fixture.connect().await.unwrap();

    let result = client
        .seal(SealRequest {
            run_id: RUN_ID.to_owned(),
            attempt_id: ATTEMPT_ID.to_owned(),
            manifest_digest: APPROVAL_DIGEST.to_owned(),
        })
        .await;

    assert!(matches!(
        result,
        Err(NativeExecutorError::OutcomeUnknown { operation: "seal" })
    ));
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "Set LOOKBACK_CODING_AGENT_EXECUTOR_BIN to a locally built native executor"]
async fn compiled_executor_keeps_the_sealed_artifact_after_workspace_removal() {
    let executor_source = std::env::var_os("LOOKBACK_CODING_AGENT_EXECUTOR_BIN")
        .expect("the ignored test requires LOOKBACK_CODING_AGENT_EXECUTOR_BIN");
    let executor_source = PathBuf::from(executor_source);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    make_private_dir(root);
    let package = root.join("package");
    make_private_dir(&package);
    let executable = package.join("coding-agent-executor");
    fs::copy(executor_source, &executable).unwrap();
    set_executable_mode(&executable);
    let home = root.join("home");
    make_private_dir(&home);
    let bare = root.join("origin.git");
    let source = root.join("source");
    make_private_dir(&source);
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

    git(
        root,
        &home,
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            bare.to_str().unwrap(),
        ],
    );
    git(&source, &home, &["init", "--initial-branch=main"]);
    fs::write(source.join("base.txt"), "approved base\n").unwrap();
    git(&source, &home, &["add", "base.txt"]);
    git(
        &source,
        &home,
        &[
            "-c",
            "user.name=Executor Test",
            "-c",
            "user.email=executor-test@invalid",
            "commit",
            "-m",
            "approved base",
        ],
    );
    git(
        &source,
        &home,
        &["remote", "add", "origin", &file_url(&bare)],
    );
    git(&source, &home, &["push", "origin", "main"]);
    let base_commit = git(&source, &home, &["rev-parse", "HEAD"]);

    let repository = RepositoryId {
        host: "localhost".to_owned(),
        owner: "owner".to_owned(),
        name: "repo".to_owned(),
    };
    let trusted_config = TrustedConfig {
        protocol_version: PROTOCOL_VERSION,
        expected_executor_version: coding_agent_executor::EXECUTOR_VERSION.to_owned(),
        workspace_root: workspace_root.clone(),
        publisher_root,
        state_root,
        credential_root,
        max_import_bytes: 1024 * 1024,
        repositories: vec![RepositoryConfig {
            id: repository.clone(),
            clone_url: file_url(&bare),
            ssh_directory: None,
            allowed_base_branches: vec!["main".to_owned()],
            read_credential_source: None,
            publish_credential_source: None,
            forge: None,
        }],
        credential_sources: Vec::new(),
    };
    let config_path = root.join("trusted-executor.json");
    write_private_config(&config_path, &trusted_config);
    let client =
        NativeExecutorClient::connect(executable.clone(), config_path, sha256_file(&executable))
            .await
            .unwrap();

    let prepared = client
        .prepare(PrepareRequest {
            issue_id: Some("42".into()),
            run_id: RUN_ID.to_owned(),
            attempt_id: ATTEMPT_ID.to_owned(),
            repository: repository.clone(),
            base_branch: "main".to_owned(),
            base_commit: base_commit.clone(),
        })
        .await
        .unwrap();
    assert_eq!(prepared.branch, format!("agent/issue-42-{RUN_ID}"));
    let workspace = PathBuf::from(&prepared.workspace_path);
    fs::write(workspace.join("captured.txt"), "frozen host artifact\n").unwrap();
    let capture = client
        .capture(CaptureRequest {
            run_id: RUN_ID.to_owned(),
            attempt_id: ATTEMPT_ID.to_owned(),
            repository: repository.clone(),
            base_branch: "main".to_owned(),
            base_commit: base_commit.clone(),
            approved_snapshot_digest: APPROVAL_DIGEST.to_owned(),
            scope_digest: SCOPE_DIGEST.to_owned(),
            intent_approval_digest: INTENT_DIGEST.to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(
        capture.capture_guarantee,
        CaptureGuarantee::HostIsolatedAfterSeal
    );
    let sealed = client
        .seal(SealRequest {
            run_id: RUN_ID.to_owned(),
            attempt_id: ATTEMPT_ID.to_owned(),
            manifest_digest: capture.manifest_digest,
        })
        .await
        .unwrap();

    fs::write(workspace.join("captured.txt"), "mutated after seal\n").unwrap();
    fs::remove_dir_all(&workspace).unwrap();
    let resolved = client
        .resolve(ResolveRequest {
            run_id: RUN_ID.to_owned(),
            attempt_id: ATTEMPT_ID.to_owned(),
            artifact_digest: sealed.artifact_digest.clone(),
        })
        .await
        .unwrap();
    assert_eq!(resolved.commit, sealed.commit);
    assert_eq!(resolved.tree, sealed.tree);

    let pushed = client
        .push(PushRequest {
            run_id: RUN_ID.to_owned(),
            attempt_id: ATTEMPT_ID.to_owned(),
            repository,
            branch: format!("agent/issue-42-{RUN_ID}"),
            base_branch: "main".to_owned(),
            base_commit,
            artifact_digest: sealed.artifact_digest,
            frozen_commit: sealed.commit.clone(),
        })
        .await
        .unwrap();
    assert_eq!(pushed.commit, sealed.commit);
    assert_eq!(pushed.remote_head, sealed.commit);
    assert_eq!(pushed.artifact_digest, resolved.artifact_digest);
    assert_eq!(
        git(
            root,
            &home,
            &[
                "--git-dir",
                bare.to_str().unwrap(),
                "show",
                &format!("{}:captured.txt", pushed.commit)
            ]
        ),
        "frozen host artifact"
    );
    assert!(!workspace.exists());
}

fn valid_capabilities() -> serde_json::Value {
    capabilities_with(
        &[
            CORE_NATIVE_V2_FEATURES,
            &["push", "pull_request", "secret_scan_supported_formats_v1"],
        ]
        .concat(),
    )
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_after_pr_enqueue_still_collects_the_accepted_response() {
    use lookback_tauri_lib::jobworkerp::JobworkerpHandle;
    use native_interactive_rpc::{RpcFixture, start_server};
    let fixture = FakeExecutor::new(
        valid_capabilities(),
        helper_error("pull_request_not_approved"),
    );
    let rpc = RpcFixture::default();
    rpc.enable_preparation_workflow();
    let server = start_server(rpc.clone()).await;
    let client = fixture
        .connect()
        .await
        .unwrap()
        .with_preparation_workflow(JobworkerpHandle::connect(&server.url).await.unwrap());
    let gate = rpc.block_next_enqueue();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let mut task = tokio::spawn({
        let cancellation = cancellation.clone();
        let request = pull_request(fixture.repository.clone());
        async move {
            client
                .create_pull_request_workflow_with_cancellation(request, &cancellation)
                .await
        }
    });
    gate.wait_until_entered().await;
    cancellation.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut task)
            .await
            .is_err()
    );
    gate.resume();
    assert_eq!(
        task.await.unwrap().unwrap_err(),
        NativeExecutorError::ExecutorRejected(ExecutorErrorCode::PullRequestNotApproved)
    );
    assert_eq!(rpc.counts().enqueue_for_stream_calls, 1);
    assert_eq!(fixture.invocation_count(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_pr_registration_or_final_worker_lookup_prevents_enqueue() {
    use lookback_tauri_lib::jobworkerp::JobworkerpHandle;
    use native_interactive_rpc::{RpcFixture, start_server};
    for prior_lookups in 0..2 {
        let fixture = FakeExecutor::new(
            valid_capabilities(),
            helper_error("pull_request_not_approved"),
        );
        let rpc = RpcFixture::default();
        rpc.enable_preparation_workflow();
        let server = start_server(rpc.clone()).await;
        let client = fixture
            .connect()
            .await
            .unwrap()
            .with_preparation_workflow(JobworkerpHandle::connect(&server.url).await.unwrap());
        let gate = rpc.pause_worker_lookup_after(prior_lookups);
        let cancellation = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn({
            let cancellation = cancellation.clone();
            let request = pull_request(fixture.repository.clone());
            async move {
                client
                    .create_pull_request_workflow_with_cancellation(request, &cancellation)
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), gate.wait_until_entered())
            .await
            .unwrap_or_else(|_| panic!("lookup stage {prior_lookups}: {:?}", rpc.counts()));
        cancellation.cancel();
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(1), task).await;
        gate.resume();
        assert!(stopped.unwrap().unwrap().is_err());
        assert_eq!(rpc.counts().enqueue_for_stream_calls, 0);
        assert_eq!(fixture.invocation_count(), 1);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn pull_request_workflow_binds_the_receipt_to_the_request_and_approved_forge() {
    use lookback_tauri_lib::jobworkerp::JobworkerpHandle;
    use native_interactive_rpc::{RpcFixture, start_server};
    for mismatch in [None, Some("attempt"), Some("commit"), Some("forge")] {
        let response = json!({"protocol_version":PROTOCOL_VERSION,"status":"ok","result":{"pull_request_created":{
            "run_id":RUN_ID,"attempt_id":if mismatch==Some("attempt") {"other"} else {ATTEMPT_ID},
            "repository":{"host":"example.test","owner":"owner","name":"repo"},"number":81,
            "url":if mismatch==Some("forge") {"https://foreign.test/owner/repo/pull/81"} else {"https://example.test/owner/repo/pull/81"},
            "head_commit":if mismatch==Some("commit") {"d".repeat(40)} else {BASE_COMMIT.into()},
            "artifact_digest":APPROVAL_DIGEST,"base_branch":"main","already_present":false,
        }}});
        let fixture = FakeExecutor::new(valid_capabilities(), response);
        let mut config: TrustedConfig =
            serde_json::from_slice(&fs::read(&fixture.config).unwrap()).unwrap();
        config.repositories[0].forge = Some(coding_agent_executor::ForgeConfig {
            kind: coding_agent_executor::ForgeKind::Gitea,
            api_base_url: "https://example.test/api/v1".into(),
            web_base_url: "https://example.test".into(),
        });
        write_private_config(&fixture.config, &config);
        let rpc = RpcFixture::default();
        rpc.enable_preparation_workflow();
        let server = start_server(rpc.clone()).await;
        let client = fixture
            .connect()
            .await
            .unwrap()
            .with_preparation_workflow(JobworkerpHandle::connect(&server.url).await.unwrap());
        let result = client
            .create_pull_request_workflow(pull_request(fixture.repository.clone()))
            .await;
        if mismatch.is_none() {
            assert_eq!(result.unwrap().number, 81);
        } else {
            assert!(matches!(
                result,
                Err(NativeExecutorError::OutcomeUnknown {
                    operation: "create_pull_request"
                })
            ));
        }
        assert_eq!(rpc.counts().enqueue_for_stream_calls, 1);
        assert_eq!(fixture.invocation_count(), 2);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn pull_request_workflow_uses_registered_dispatch_and_never_falls_back_to_direct_execution() {
    use lookback_tauri_lib::jobworkerp::JobworkerpHandle;
    use native_interactive_rpc::{RpcFixture, start_server};
    let fixture = FakeExecutor::new(
        valid_capabilities(),
        helper_error("pull_request_not_approved"),
    );
    let client = fixture.connect().await.unwrap();
    let request = pull_request(fixture.repository.clone());
    assert_eq!(
        client
            .create_pull_request_workflow(request.clone())
            .await
            .unwrap_err(),
        NativeExecutorError::Unavailable
    );
    assert_eq!(fixture.invocation_count(), 1);
    let rpc = RpcFixture::default();
    rpc.enable_preparation_workflow();
    let server = start_server(rpc.clone()).await;
    let client =
        client.with_preparation_workflow(JobworkerpHandle::connect(&server.url).await.unwrap());
    assert_eq!(
        client
            .create_pull_request_workflow(request.clone())
            .await
            .unwrap_err(),
        NativeExecutorError::ExecutorRejected(ExecutorErrorCode::PullRequestNotApproved)
    );
    assert_eq!(rpc.counts().enqueue_for_stream_calls, 1);
    assert_eq!(fixture.invocation_count(), 2);
    let mut invalid = request.clone();
    invalid.frozen_commit = "invalid-commit".into();
    assert_eq!(
        client
            .create_pull_request_workflow(invalid)
            .await
            .unwrap_err(),
        NativeExecutorError::InvalidRequest
    );
    assert_eq!(rpc.counts().enqueue_for_stream_calls, 1);
    rpc.set_enqueue_unavailable();
    assert!(matches!(
        client.create_pull_request_workflow(request).await,
        Err(NativeExecutorError::Unavailable | NativeExecutorError::OutcomeUnknown { .. })
    ));
    assert_eq!(rpc.counts().enqueue_for_stream_calls, 2);
    assert_eq!(fixture.invocation_count(), 3);
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_publication_requires_its_capability_and_registered_workflow_without_direct_fallback()
 {
    use coding_agent_executor::WorkspacePushRequest;
    use lookback_tauri_lib::jobworkerp::JobworkerpHandle;
    use native_interactive_rpc::{RpcFixture, start_server};
    let request = |repository| WorkspacePushRequest {
        source: prepare_request(repository),
        expected_commit: "d".repeat(40),
        source_digest: APPROVAL_DIGEST.into(),
    };
    let legacy = FakeExecutor::new(valid_capabilities(), helper_error("invalid_request"));
    let client = legacy.connect().await.unwrap();
    assert_eq!(
        client
            .push_workspace(request(legacy.repository.clone()))
            .await
            .unwrap_err(),
        NativeExecutorError::UnsupportedCapability {
            capability: "push_workspace"
        }
    );
    assert_eq!(legacy.invocation_count(), 1);
    let capabilities = capabilities_with(
        &[
            CORE_NATIVE_V2_FEATURES,
            &["push", "push_workspace", "secret_scan_supported_formats_v1"],
        ]
        .concat(),
    );
    let source = request(legacy.repository.clone());
    let receipt = json!({"protocol_version":PROTOCOL_VERSION,"status":"ok","result":{"pushed":{
        "run_id":RUN_ID,"attempt_id":ATTEMPT_ID,"repository":legacy.repository,"branch":source.source.expected_branch(),
        "commit":source.expected_commit,"remote_head":source.expected_commit,"artifact_digest":source.source_digest,
        "secret_scan":"clean_supported_formats_v1","already_present":false,
    }}});
    let fixture = FakeExecutor::new(capabilities, receipt.clone());
    let client = fixture.connect().await.unwrap();
    assert_eq!(
        client
            .push_workspace(request(fixture.repository.clone()))
            .await
            .unwrap_err(),
        NativeExecutorError::Unavailable
    );
    assert_eq!(fixture.invocation_count(), 1);
    let rpc = RpcFixture::default();
    rpc.enable_preparation_workflow();
    let server = start_server(rpc.clone()).await;
    let client =
        client.with_preparation_workflow(JobworkerpHandle::connect(&server.url).await.unwrap());
    let published = client
        .push_workspace(request(fixture.repository.clone()))
        .await
        .unwrap();
    assert_eq!(published.commit, source.expected_commit);
    assert_eq!(published.branch, source.source.expected_branch());
    assert_eq!(rpc.counts().enqueue_for_stream_calls, 1);
    assert_eq!(fixture.invocation_count(), 2);
    let mut invalid = request(fixture.repository.clone());
    invalid.source_digest = "bad".into();
    assert_eq!(
        client.push_workspace(invalid).await.unwrap_err(),
        NativeExecutorError::InvalidRequest
    );
    assert_eq!(rpc.counts().enqueue_for_stream_calls, 1);
    rpc.set_enqueue_unavailable();
    assert!(matches!(
        client
            .push_workspace(request(fixture.repository.clone()))
            .await,
        Err(NativeExecutorError::OutcomeUnknown { .. })
    ));
    // The fixture executes the workflow before losing its admission response.
    assert_eq!(fixture.invocation_count(), 3);
    assert_eq!(rpc.counts().enqueue_for_stream_calls, 2);
}

fn current_capabilities() -> serde_json::Value {
    capabilities_with(
        &[
            CORE_NATIVE_V2_FEATURES,
            &[
                "push",
                "pull_request",
                "secret_scan_supported_formats_v1",
                "resolve_base",
            ],
        ]
        .concat(),
    )
}

fn core_capabilities() -> serde_json::Value {
    capabilities_with(CORE_NATIVE_V2_FEATURES)
}

fn capabilities_with(features: &[&str]) -> serde_json::Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "executor_version": coding_agent_executor::EXECUTOR_VERSION,
        "features": features
    })
}

fn helper_error(code: &str) -> serde_json::Value {
    let (message, retryable) = match code {
        "invalid_request" => ("The request is invalid.", false),
        "workspace_not_prepared" => (
            "The run has no completed trusted workspace preparation.",
            false,
        ),
        "pull_request_not_approved" => (
            "Pull-request creation is not configured for this repository.",
            false,
        ),
        "forge_unavailable" => (
            "The configured forge could not be reconciled; retry may be safe.",
            true,
        ),
        "io_failure" => ("A required host-side file operation failed.", true),
        "response_too_large" => (
            "A forge response exceeded the configured safety limit.",
            false,
        ),
        _ => panic!("test fixture error code is not mapped"),
    };
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "status": "error",
        "error": {"code": code, "message": message, "retryable": retryable}
    })
}

fn prepare_request(repository: RepositoryId) -> PrepareRequest {
    PrepareRequest {
        issue_id: None,
        run_id: RUN_ID.to_owned(),
        attempt_id: ATTEMPT_ID.to_owned(),
        repository,
        base_branch: "main".to_owned(),
        base_commit: BASE_COMMIT.to_owned(),
    }
}

fn push_request(repository: RepositoryId) -> PushRequest {
    PushRequest {
        run_id: RUN_ID.to_owned(),
        attempt_id: ATTEMPT_ID.to_owned(),
        repository,
        branch: format!("agent/{RUN_ID}"),
        base_branch: "main".to_owned(),
        base_commit: BASE_COMMIT.to_owned(),
        artifact_digest: APPROVAL_DIGEST.to_owned(),
        frozen_commit: BASE_COMMIT.to_owned(),
    }
}

fn resolve_base_request(repository: RepositoryId) -> ResolveBaseRequest {
    ResolveBaseRequest {
        run_id: RUN_ID.to_owned(),
        attempt_id: ATTEMPT_ID.to_owned(),
        operation_id: "operation-1".to_owned(),
        repository,
        base_branch: "main".to_owned(),
    }
}

fn pull_request(repository: RepositoryId) -> CreatePullRequestRequest {
    CreatePullRequestRequest {
        run_id: RUN_ID.to_owned(),
        attempt_id: ATTEMPT_ID.to_owned(),
        repository,
        branch: format!("agent/{RUN_ID}"),
        base_branch: "main".to_owned(),
        base_commit: BASE_COMMIT.to_owned(),
        artifact_digest: APPROVAL_DIGEST.to_owned(),
        frozen_commit: BASE_COMMIT.to_owned(),
        pull_request: PullRequestRequest {
            title: "Native executor test".to_owned(),
            body: "Local test only".to_owned(),
        },
    }
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn write_config(root: &Path, clone_url: Option<String>) -> (PathBuf, RepositoryId) {
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
        host: "example.test".to_owned(),
        owner: "owner".to_owned(),
        name: "repo".to_owned(),
    };
    let config = TrustedConfig {
        protocol_version: PROTOCOL_VERSION,
        expected_executor_version: coding_agent_executor::EXECUTOR_VERSION.to_owned(),
        workspace_root,
        publisher_root,
        state_root,
        credential_root,
        max_import_bytes: 1024 * 1024,
        repositories: vec![RepositoryConfig {
            id: repository.clone(),
            clone_url: clone_url
                .unwrap_or_else(|| "https://example.test/owner/repo.git".to_owned()),
            ssh_directory: None,
            allowed_base_branches: vec!["main".to_owned()],
            read_credential_source: None,
            publish_credential_source: None,
            forge: None,
        }],
        credential_sources: Vec::new(),
    };
    let config_path = root.join("trusted-executor.json");
    write_private_config(&config_path, &config);
    (config_path, repository)
}

#[cfg(unix)]
fn write_private_config(path: &Path, config: &TrustedConfig) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, serde_json::to_vec(config).unwrap()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(unix)]
fn make_private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(target_os = "linux")]
fn set_executable_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path).unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(target_os = "linux")]
fn git(cwd: &Path, home: &Path, args: &[&str]) -> String {
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
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

#[cfg(target_os = "linux")]
fn file_url(path: &Path) -> String {
    url::Url::from_file_path(path).unwrap().to_string()
}

#[cfg(unix)]
#[tokio::test]
async fn lookup_publication_requires_its_own_capability_and_binds_remote_receipts() {
    let unsupported = FakeExecutor::new(core_capabilities(), helper_error("invalid_request"));
    let client = unsupported.connect().await.unwrap();
    assert_eq!(
        client
            .lookup_push(push_request(unsupported.repository.clone()))
            .await
            .unwrap_err(),
        NativeExecutorError::UnsupportedCapability {
            capability: "lookup_publication"
        }
    );
    assert_eq!(
        client
            .lookup_pull_request(pull_request(unsupported.repository.clone()))
            .await
            .unwrap_err(),
        NativeExecutorError::UnsupportedCapability {
            capability: "lookup_publication"
        }
    );
    assert_eq!(unsupported.invocation_count(), 1);
    for mismatch in [None, Some("attempt"), Some("commit"), Some("digest")] {
        let response = json!({"protocol_version":PROTOCOL_VERSION,"status":"ok","result":{"pushed":{
            "run_id":RUN_ID,"attempt_id":if mismatch==Some("attempt") {"other"} else {ATTEMPT_ID},
            "repository":unsupported.repository,"branch":format!("agent/{RUN_ID}"),
            "commit":if mismatch==Some("commit") {"d".repeat(40)} else {BASE_COMMIT.into()},
            "remote_head":BASE_COMMIT,"artifact_digest":if mismatch==Some("digest") {SCOPE_DIGEST} else {APPROVAL_DIGEST},
            "secret_scan":"clean_supported_formats_v1","already_present":true
        }}});
        let fixture = FakeExecutor::new(
            capabilities_with(&["prepare", "lookup_publication"]),
            response,
        );
        let client = fixture.connect().await.unwrap();
        let request = push_request(fixture.repository.clone());
        let result = client.lookup_push(request).await;
        if mismatch.is_none() {
            assert_eq!(result.unwrap().remote_head, BASE_COMMIT);
        } else {
            assert!(matches!(
                result,
                Err(NativeExecutorError::OutcomeUnknown {
                    operation: "lookup_push"
                })
            ));
        }
        let mut invalid = push_request(fixture.repository.clone());
        invalid.frozen_commit = "bad".into();
        assert_eq!(
            client.lookup_push(invalid).await.unwrap_err(),
            NativeExecutorError::InvalidRequest
        );
        assert_eq!(fixture.invocation_count(), 2);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn lookup_pull_request_binds_the_actual_forge_and_base_without_publication_capabilities() {
    for mismatch in [
        None,
        Some("attempt"),
        Some("commit"),
        Some("forge"),
        Some("base"),
    ] {
        let response = json!({"protocol_version":PROTOCOL_VERSION,"status":"ok","result":{"pull_request_created":{
            "run_id":RUN_ID,"attempt_id":if mismatch==Some("attempt") {"other"} else {ATTEMPT_ID},
            "repository":{"host":"example.test","owner":"owner","name":"repo"},"number":81,
            "url":if mismatch==Some("forge") {"https://foreign.test/owner/repo/pull/81"} else {"https://example.test/owner/repo/pull/81"},
            "head_commit":if mismatch==Some("commit") {"d".repeat(40)} else {BASE_COMMIT.into()},
            "artifact_digest":APPROVAL_DIGEST,"base_branch":if mismatch==Some("base") {"trunk"} else {"main"},"already_present":true
        }}});
        let fixture = FakeExecutor::new(
            capabilities_with(&["prepare", "lookup_publication"]),
            response,
        );
        let mut config: TrustedConfig =
            serde_json::from_slice(&fs::read(&fixture.config).unwrap()).unwrap();
        config.repositories[0].forge = Some(coding_agent_executor::ForgeConfig {
            kind: coding_agent_executor::ForgeKind::Gitea,
            api_base_url: "https://example.test/api/v1".into(),
            web_base_url: "https://example.test".into(),
        });
        write_private_config(&fixture.config, &config);
        let client = fixture.connect().await.unwrap();
        let result = client
            .lookup_pull_request(pull_request(fixture.repository.clone()))
            .await;
        if mismatch.is_none() {
            assert_eq!(result.unwrap().number, 81);
        } else {
            assert!(matches!(
                result,
                Err(NativeExecutorError::OutcomeUnknown {
                    operation: "lookup_pull_request"
                })
            ));
        }
        assert_eq!(fixture.invocation_count(), 2);
    }
}
