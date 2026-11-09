use std::path::PathBuf;

use coding_agent_executor::RepositoryId;
use jobworkerp_client::{
    jobworkerp::{
        data::{JobId, ResultOutputItem, WorkerId, result_output_item::Item},
        service::job_request,
    },
    proto::JobworkerpProto,
};
use lookback_tauri_lib::{
    grpc::proto::jobworkerp::runner::sandbox::{
        SandboxExecExit, SandboxExecOutput, SandboxExecResult,
        sandbox_exec_result::Result as SandboxResult,
    },
    jobworkerp::{
        JobworkerpHandle,
        coding_agent::{
            HostWorkerContext, NativeInteractiveWorkerSpec, NativeVersionProbeOptions,
            NativeWorkerMatch, NativeWorkspaceBinding,
        },
    },
};
use prost::Message;
use serde::Serialize as _;
use serde_json::json;
use tokio::time::Duration;

#[path = "fixtures/native_interactive_rpc.rs"]
mod native_interactive_rpc;

use native_interactive_rpc::{RpcFixture, TestServer, start_server};

const WRONG_EXIT_TAG_RESULT_PROTO: &str = r#"
    syntax = "proto3";
    package jobworkerp.runner;
    message SandboxExecResult {
      oneof result { SandboxExecOutput output = 1; SandboxExecExit exit = 2; }
    }
    message SandboxExecOutput { string stream = 1; bytes data = 2; }
    message SandboxExecExit {
      int32 exit_code = 4;
      uint64 execution_time_ms = 2;
      string sandbox_id = 3;
    }
"#;

const WRONG_EXIT_TYPE_RESULT_PROTO: &str = r#"
    syntax = "proto3";
    package jobworkerp.runner;
    message SandboxExecResult {
      oneof result { SandboxExecOutput output = 1; SandboxExecExit exit = 2; }
    }
    message SandboxExecOutput { string stream = 1; bytes data = 2; }
    message SandboxExecExit {
      uint32 exit_code = 1;
      uint64 execution_time_ms = 2;
      string sandbox_id = 3;
    }
"#;

const SPLIT_RESULT_VARIANTS_PROTO: &str = r#"
    syntax = "proto3";
    package jobworkerp.runner;
    message SandboxExecResult {
      SandboxExecOutput output = 1;
      SandboxExecExit exit = 2;
    }
    message SandboxExecOutput { string stream = 1; bytes data = 2; }
    message SandboxExecExit {
      int32 exit_code = 1;
      uint64 execution_time_ms = 2;
      string sandbox_id = 3;
    }
"#;

const OPTIONAL_EXIT_CODE_RESULT_PROTO: &str = r#"
    syntax = "proto3";
    package jobworkerp.runner;
    message SandboxExecResult {
      oneof result { SandboxExecOutput output = 1; SandboxExecExit exit = 2; }
    }
    message SandboxExecOutput { string stream = 1; bytes data = 2; }
    message SandboxExecExit {
      optional int32 exit_code = 1;
      uint64 execution_time_ms = 2;
      string sandbox_id = 3;
    }
"#;

const WRONG_OUTPUT_TAG_RESULT_PROTO: &str = r#"
    syntax = "proto3";
    package jobworkerp.runner;
    message SandboxExecResult {
      oneof result { SandboxExecOutput output = 1; SandboxExecExit exit = 2; }
    }
    message SandboxExecOutput { string stream = 1; bytes data = 3; }
    message SandboxExecExit {
      int32 exit_code = 1;
      uint64 execution_time_ms = 2;
      string sandbox_id = 3;
    }
"#;

const RENAMED_COMPATIBLE_RESULT_PROTO: &str = r#"
    syntax = "proto3";
    package sandbox_wire.v2;
    message ProbeResult {
      oneof payload {
        ProcessOutput stdout_event = 1;
        ProcessExit status_event = 2;
      }
    }
    message ProcessOutput { string channel = 1; bytes payload = 2; }
    message ProcessExit {
      int32 status = 1;
      uint64 elapsed_ms = 2;
      string instance = 3;
    }
"#;

fn set_result_schema(fixture: &RpcFixture, schema: &str) {
    fixture.mutate_runner_data(|runner| {
        for method in runner
            .method_proto_map
            .as_mut()
            .unwrap()
            .schemas
            .values_mut()
        {
            method.result_proto = schema.to_owned();
        }
    });
}

fn spec() -> NativeInteractiveWorkerSpec {
    spec_with_static(true)
}

fn spec_with_static(use_static: bool) -> NativeInteractiveWorkerSpec {
    NativeInteractiveWorkerSpec {
        native_profile_id: "opencode-native".into(),
        repository_id: RepositoryId {
            host: "forge.example".into(),
            owner: "team".into(),
            name: "repo".into(),
        },
        use_static,
        workspace_binding: NativeWorkspaceBinding {
            canonical_host_root: PathBuf::from("/home/test/workspaces/team/repo"),
            guest_root: "/workspace".into(),
        },
        runner_settings_json: json!({
            "vm": {
                "image": "registry.example/agents/opencode@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "cpus": 1,
                "memory_mib": 2048,
                "root_disk_mib": 8192,
                "mounts": [{
                    "host_path": "/home/test/workspaces/team/repo",
                    "guest_path": "/workspace",
                    "writable": true,
                    "executable": true
                }],
                "env": {"HOME": "/home/codespace"}
            },
            "allowed_images": ["registry.example/agents/opencode@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
            "allowed_host_mounts": [{
                "host_root": "/home/test/workspaces/team/repo",
                "allow_write": true,
                "allow_exec": true
            }],
            "network": {"enabled": false}
        }),
        host_context: HostWorkerContext {
            microsandbox_sdk_home: PathBuf::from("/home/test/.local/share/lookback/msb"),
            backend_config_path: PathBuf::from("/home/test/.local/share/lookback/msb/config.json"),
        },
    }
}

async fn registered(fixture: RpcFixture) -> (TestServer, JobworkerpHandle, NativeWorkerMatch) {
    registered_with_spec(fixture, spec()).await
}

async fn registered_with_spec(
    fixture: RpcFixture,
    worker_spec: NativeInteractiveWorkerSpec,
) -> (TestServer, JobworkerpHandle, NativeWorkerMatch) {
    let server = start_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let matched = handle
        .ensure_native_interactive_worker(worker_spec)
        .await
        .unwrap();
    (server, handle, matched)
}

fn sandbox_data(result: SandboxResult) -> ResultOutputItem {
    ResultOutputItem {
        item: Some(Item::Data(
            SandboxExecResult {
                result: Some(result),
            }
            .encode_to_vec(),
        )),
    }
}

async fn assert_rejected_before_match(fixture: RpcFixture) {
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    assert!(
        handle
            .ensure_native_interactive_worker(spec())
            .await
            .is_err()
    );
    assert!(fixture.job_requests().is_empty());
    assert_eq!(fixture.counts().create_calls, 0);
    assert_eq!(fixture.counts().job_enqueue_calls, 0);
}

#[tokio::test]
async fn version_probe_is_one_shot_schema_bound_and_returns_raw_version_output_and_job_id() {
    let fixture = RpcFixture::default();
    fixture.set_job_result_items(vec![
        ResultOutputItem {
            item: Some(Item::Data(
                SandboxExecResult {
                    result: Some(SandboxResult::Output(SandboxExecOutput {
                        stream: "stdout".into(),
                        data: b"1.18.3\n".to_vec(),
                    })),
                }
                .encode_to_vec(),
            )),
        },
        ResultOutputItem {
            item: Some(Item::Data(
                SandboxExecResult {
                    result: Some(SandboxResult::Exit(SandboxExecExit {
                        exit_code: 0,
                        execution_time_ms: 16,
                        sandbox_id: "probe-sandbox".into(),
                    })),
                }
                .encode_to_vec(),
            )),
        },
        ResultOutputItem {
            item: Some(Item::End(Default::default())),
        },
    ]);
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let matched = handle
        .ensure_native_interactive_worker(spec())
        .await
        .unwrap();

    let mut probe = handle
        .start_native_version_probe(&matched, "/workspace/project")
        .await
        .unwrap();

    assert_eq!(probe.job_id(), &JobId { value: 9_004 });
    assert_eq!(probe.worker_id(), matched.worker_id());
    assert_eq!(probe.settings_digest(), matched.settings_digest());
    assert_eq!(
        probe.matched_runner_schema_digest(),
        matched.method_schema_digest()
    );
    assert_eq!(probe.run_schema_digest().len(), 64);
    assert!(probe.deadline() > tokio::time::Instant::now());

    let request = fixture
        .job_requests()
        .pop()
        .expect("one probe was enqueued");
    assert_eq!(
        request.worker,
        Some(job_request::Worker::WorkerId(WorkerId {
            value: matched.worker_id().value,
        }))
    );
    assert_eq!(request.using.as_deref(), Some("run"));
    assert_eq!(request.timeout, Some(60_000));
    assert!(request.uniq_key.is_none());
    assert!(request.overrides.is_none());
    assert_eq!(fixture.grpc_timeout_headers(), vec![None]);

    let descriptor =
        JobworkerpProto::parse_job_args_schema_descriptor(&fixture.runner_data(), Some("run"))
            .unwrap()
            .unwrap();
    let decoded = prost_reflect::DynamicMessage::decode(descriptor, request.args.as_slice())
        .unwrap()
        .serialize(serde_json::value::Serializer)
        .unwrap();
    assert_eq!(decoded["command"], "opencode");
    assert_eq!(decoded["args"], json!(["--version"]));
    assert_eq!(decoded["workingDir"], "/workspace/project");
    assert_eq!(decoded.as_object().unwrap().len(), 3);

    assert_eq!(
        probe.next_event().await.unwrap(),
        Some(
            lookback_tauri_lib::jobworkerp::coding_agent::CodingAgentEvent::Output {
                stream: "stdout".into(),
                data: b"1.18.3\n".to_vec(),
            }
        )
    );
    assert_eq!(
        probe.next_event().await.unwrap(),
        Some(
            lookback_tauri_lib::jobworkerp::coding_agent::CodingAgentEvent::Exit {
                exit_code: 0,
                execution_time_ms: 16,
                sandbox_id: "probe-sandbox".into(),
            }
        )
    );
    assert_eq!(probe.next_event().await.unwrap(), None);
    assert_eq!(fixture.counts().create_calls, 1);
    assert_eq!(fixture.counts().job_enqueue_calls, 1);
    assert_eq!(fixture.counts().enqueue_for_stream_calls, 1);
    assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 0);
    assert_eq!(fixture.counts().enqueue_for_stream_calls, 1);
    assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 0);
    assert_eq!(fixture.counts().job_delete_calls, 0);
    assert_eq!(fixture.counts().worker_delete_calls, 0);
    assert_eq!(fixture.counts().release_static_calls, 0);

    let options = NativeVersionProbeOptions::default();
    assert_eq!(options.timeout, Duration::from_secs(60));
}

#[tokio::test]
async fn prepared_cwd_must_be_normalized_and_component_contained_by_the_matched_workspace() {
    let fixture = RpcFixture::default();
    let (_server, handle, matched) = registered(fixture.clone()).await;

    for cwd in [
        "workspace/project",
        "/workspace/../outside",
        "/workspace-other/project",
        "/workspace//project",
        "/workspace/project/",
        "/tmp/outside",
    ] {
        assert!(
            handle
                .start_native_version_probe(&matched, cwd)
                .await
                .is_err(),
            "invalid prepared cwd must be rejected: {cwd}"
        );
    }
    assert!(fixture.job_requests().is_empty());
    assert_eq!(fixture.counts().job_enqueue_calls, 0);
}

#[tokio::test]
async fn worker_match_rejects_a_run_method_that_is_missing_streaming_or_has_unusable_schema() {
    for problem in ["missing", "client_stream", "non_streaming", "bad_args"] {
        let fixture = RpcFixture::default();
        fixture.mutate_runner_data(|runner| {
            let methods = &mut runner.method_proto_map.as_mut().unwrap().schemas;
            match problem {
                "missing" => {
                    let _ = methods.remove("run");
                }
                "client_stream" => methods.get_mut("run").unwrap().require_client_stream = true,
                "non_streaming" => {
                    methods.get_mut("run").unwrap().output_type =
                        jobworkerp_client::jobworkerp::data::StreamingOutputType::NonStreaming
                            as i32;
                }
                "bad_args" => {
                    methods.get_mut("run").unwrap().args_proto = r#"
                        syntax = "proto3";
                        package jobworkerp.runner;
                        message SandboxExecArgs {
                          string command = 1;
                          repeated string args = 2;
                        }
                    "#
                    .to_owned();
                }
                _ => unreachable!(),
            }
        });

        assert_rejected_before_match(fixture).await;
    }
}

#[tokio::test]
async fn same_named_result_schemas_with_incompatible_wire_contract_are_rejected_before_match() {
    for schema in [
        WRONG_EXIT_TAG_RESULT_PROTO,
        WRONG_EXIT_TYPE_RESULT_PROTO,
        SPLIT_RESULT_VARIANTS_PROTO,
        OPTIONAL_EXIT_CODE_RESULT_PROTO,
        WRONG_OUTPUT_TAG_RESULT_PROTO,
    ] {
        let fixture = RpcFixture::default();
        set_result_schema(&fixture, schema);
        let server = start_server(fixture.clone()).await;
        let handle = JobworkerpHandle::connect(&server.url).await.unwrap();

        assert!(
            handle
                .ensure_native_interactive_worker(spec())
                .await
                .is_err(),
            "result schemas incompatible with the fixed Prost decoder must be rejected"
        );
        assert_eq!(fixture.counts().create_calls, 0);
        assert_eq!(fixture.counts().job_enqueue_calls, 0);
    }
}

#[tokio::test]
async fn incompatible_run_result_schema_prevents_native_worker_match_success() {
    let fixture = RpcFixture::default();
    fixture.mutate_runner_data(|runner| {
        runner
            .method_proto_map
            .as_mut()
            .unwrap()
            .schemas
            .get_mut("run")
            .unwrap()
            .result_proto = WRONG_EXIT_TAG_RESULT_PROTO.to_owned();
    });
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();

    assert!(
        handle
            .ensure_native_interactive_worker(spec())
            .await
            .is_err()
    );
    assert_eq!(fixture.counts().create_calls, 0);
    assert_eq!(fixture.counts().job_enqueue_calls, 0);
}

#[tokio::test]
async fn result_message_and_field_renames_are_accepted_when_wire_shape_is_unchanged() {
    let fixture = RpcFixture::default();
    set_result_schema(&fixture, RENAMED_COMPATIBLE_RESULT_PROTO);
    let (_server, handle, matched) = registered(fixture.clone()).await;

    let probe = handle
        .start_native_version_probe(&matched, "/workspace")
        .await
        .unwrap();

    assert_eq!(probe.worker_id(), matched.worker_id());
    assert_eq!(probe.run_schema_digest().len(), 64);
    assert_eq!(fixture.counts().create_calls, 1);
    assert_eq!(fixture.counts().job_enqueue_calls, 1);
}

#[tokio::test]
async fn live_runner_schema_drift_after_matching_is_rejected_before_probe_enqueue() {
    for method_name in ["run", "run_with_client"] {
        let fixture = RpcFixture::default();
        let (_server, handle, matched) = registered(fixture.clone()).await;
        fixture.mutate_runner_data(|runner| {
            runner
                .method_proto_map
                .as_mut()
                .unwrap()
                .schemas
                .get_mut(method_name)
                .unwrap()
                .description = Some("changed live contract".to_owned());
        });

        assert!(
            handle
                .start_native_version_probe(&matched, "/workspace")
                .await
                .is_err(),
            "changed {method_name} schema is part of the matched contract"
        );
        assert!(fixture.job_requests().is_empty());
        assert_eq!(fixture.counts().job_enqueue_calls, 0);
    }
}

#[tokio::test]
async fn worker_identity_settings_and_policy_are_revalidated_before_probe_enqueue() {
    for drift in ["name", "runner_id", "settings", "policy", "worker_id"] {
        let fixture = RpcFixture::default();
        let (_server, handle, matched) = registered(fixture.clone()).await;
        match drift {
            "name" => fixture.mutate_first_worker(|worker| worker.name = "substituted".into()),
            "runner_id" => {
                fixture.mutate_first_worker(|worker| {
                    worker.runner_id =
                        Some(jobworkerp_client::jobworkerp::data::RunnerId { value: 99 });
                });
            }
            "settings" => {
                let mut settings = spec().runner_settings_json;
                settings["vm"]["memory_mib"] = json!(3072);
                let descriptor = JobworkerpProto::parse_runner_settings_schema_descriptor(
                    &fixture.runner_data(),
                )
                .unwrap()
                .unwrap();
                let bytes =
                    JobworkerpProto::json_value_to_message(descriptor, &settings, true, false)
                        .unwrap();
                fixture.mutate_first_worker(|worker| worker.runner_settings = bytes);
            }
            "policy" => fixture.mutate_first_worker(|worker| worker.use_static = false),
            "worker_id" => fixture.replace_first_worker_id(WorkerId { value: 81 }),
            _ => unreachable!(),
        }

        assert!(
            handle
                .start_native_version_probe(&matched, "/workspace")
                .await
                .is_err(),
            "changed {drift} must be rejected"
        );
        assert!(fixture.job_requests().is_empty());
        assert_eq!(fixture.counts().job_enqueue_calls, 0);
    }
}

#[tokio::test]
async fn enqueue_errors_and_bad_job_id_headers_are_never_retried() {
    for failure in ["unavailable", "missing_id", "invalid_id"] {
        let fixture = RpcFixture::default();
        match failure {
            "unavailable" => fixture.set_enqueue_unavailable(),
            "missing_id" => fixture.omit_next_job_id(),
            "invalid_id" => fixture.return_invalid_next_job_id(),
            _ => unreachable!(),
        }
        let (_server, handle, matched) = registered(fixture.clone()).await;

        assert!(
            handle
                .start_native_version_probe(&matched, "/workspace")
                .await
                .is_err()
        );
        assert_eq!(fixture.counts().job_enqueue_calls, 1);
        assert_eq!(fixture.counts().enqueue_for_stream_calls, 1);
        assert_eq!(fixture.counts().create_calls, 1);
        assert_eq!(fixture.job_requests().len(), 1);
        assert_eq!(fixture.counts().job_delete_calls, 0);
        assert_eq!(fixture.counts().worker_delete_calls, 0);
        assert_eq!(fixture.counts().release_static_calls, 0);
    }
}

#[tokio::test]
async fn probe_deadline_bounds_output_drain_without_automatically_deleting_the_job() {
    let fixture = RpcFixture::default();
    let output_gate = fixture.block_next_output();
    let (_server, handle, matched) = registered(fixture.clone()).await;
    let mut probe = handle
        .start_native_version_probe_with_options(
            &matched,
            "/workspace",
            NativeVersionProbeOptions {
                timeout: Duration::from_millis(500),
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), output_gate.wait_until_entered())
        .await
        .expect("fixture held the output stream open");

    let error = probe.next_event().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("native version probe deadline exceeded")
    );
    output_gate.resume();
    assert_eq!(fixture.counts().job_enqueue_calls, 1);
    assert_eq!(fixture.counts().job_delete_calls, 0);
    assert_eq!(fixture.counts().worker_delete_calls, 0);
    assert_eq!(fixture.counts().release_static_calls, 0);
}

#[tokio::test]
async fn probe_deadline_includes_enqueue_open_and_does_not_retry_unknown_acceptance() {
    let fixture = RpcFixture::default();
    let enqueue_gate = fixture.block_next_enqueue();
    let (_server, handle, matched) = registered(fixture.clone()).await;
    let start = handle.start_native_version_probe_with_options(
        &matched,
        "/workspace",
        NativeVersionProbeOptions {
            timeout: Duration::from_secs(2),
        },
    );
    tokio::pin!(start);
    tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            result = &mut start => panic!("probe unexpectedly finished before enqueue gate: {result:?}"),
            _ = enqueue_gate.wait_until_entered() => {}
        }
    })
    .await
    .expect("enqueue RPC reached the fixture before the overall deadline");
    let result = tokio::time::timeout(Duration::from_secs(3), &mut start)
        .await
        .expect("probe deadline bounded the blocked enqueue")
        .unwrap_err();
    assert!(
        result
            .to_string()
            .contains("native version probe deadline exceeded")
    );
    enqueue_gate.resume();
    assert_eq!(fixture.counts().job_enqueue_calls, 1);
    assert_eq!(fixture.counts().enqueue_for_stream_calls, 1);
    assert_eq!(fixture.job_requests()[0].timeout, Some(60_000));
    assert_eq!(fixture.job_requests().len(), 1);
}

#[tokio::test]
async fn probe_and_interactive_session_share_the_matched_worker_without_worker_mutations() {
    let fixture = RpcFixture::default();
    let (_server, handle, matched) =
        registered_with_spec(fixture.clone(), spec_with_static(false)).await;
    let probe = handle
        .start_native_version_probe(&matched, "/workspace")
        .await
        .unwrap();
    let session = handle
        .start_native_interactive_session(&matched, "opencode", &[], "/workspace")
        .await
        .unwrap();

    assert_eq!(probe.worker_id(), matched.worker_id());
    assert!(!fixture.created_workers()[0].use_static);
    assert_eq!(session.job_id().value, 9_005);
    assert_eq!(fixture.counts().create_calls, 1);
    assert_eq!(fixture.counts().upsert_calls, 0);
    assert_eq!(fixture.counts().worker_delete_calls, 0);
    assert_eq!(fixture.counts().release_static_calls, 0);
    assert_eq!(fixture.counts().job_enqueue_calls, 2);
    assert_eq!(fixture.counts().enqueue_for_stream_calls, 1);
    assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 1);
    let requests = fixture.job_requests();
    assert_eq!(requests[0].using.as_deref(), Some("run"));
    assert_eq!(requests[0].timeout, Some(60_000));
    assert_eq!(requests[1].using.as_deref(), Some("run_with_client"));
    assert_eq!(requests[1].timeout, Some(0));
}

#[tokio::test]
async fn probe_timeout_options_are_bounded_and_nonzero() {
    let fixture = RpcFixture::default();
    let (_server, handle, matched) = registered(fixture.clone()).await;
    for timeout in [Duration::ZERO, Duration::from_secs(61)] {
        assert!(
            handle
                .start_native_version_probe_with_options(
                    &matched,
                    "/workspace",
                    NativeVersionProbeOptions { timeout },
                )
                .await
                .is_err()
        );
    }
    assert!(fixture.job_requests().is_empty());
    assert_eq!(fixture.counts().job_enqueue_calls, 0);
}

#[tokio::test]
async fn nonzero_sandbox_exit_is_returned_as_an_exit_event_not_promoted_to_success() {
    let fixture = RpcFixture::default();
    fixture.set_job_result_items(vec![
        sandbox_data(SandboxResult::Exit(SandboxExecExit {
            exit_code: 17,
            execution_time_ms: 22,
            sandbox_id: "probe-sandbox".into(),
        })),
        ResultOutputItem {
            item: Some(Item::End(Default::default())),
        },
    ]);
    let (_server, handle, matched) = registered(fixture.clone()).await;
    let mut probe = handle
        .start_native_version_probe(&matched, "/workspace")
        .await
        .unwrap();

    assert_eq!(
        probe.next_event().await.unwrap(),
        Some(
            lookback_tauri_lib::jobworkerp::coding_agent::CodingAgentEvent::Exit {
                exit_code: 17,
                execution_time_ms: 22,
                sandbox_id: "probe-sandbox".into(),
            }
        )
    );
    assert_eq!(probe.next_event().await.unwrap(), None);
}

#[tokio::test]
async fn selected_agent_version_probe_uses_only_the_selected_literal_program() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let matched = handle
        .ensure_native_interactive_worker(spec())
        .await
        .unwrap();
    for command in ["opencode", "codex", "claude"] {
        let _probe = handle
            .start_native_agent_version_probe(
                &matched,
                "/workspace",
                command,
                NativeVersionProbeOptions::default(),
            )
            .await
            .unwrap();
        let request = fixture.job_requests().pop().unwrap();
        let descriptor =
            JobworkerpProto::parse_job_args_schema_descriptor(&fixture.runner_data(), Some("run"))
                .unwrap()
                .unwrap();
        let decoded = prost_reflect::DynamicMessage::decode(descriptor, request.args.as_slice())
            .unwrap()
            .serialize(serde_json::value::Serializer)
            .unwrap();
        assert_eq!(decoded["command"], command);
        assert_eq!(decoded["args"], json!(["--version"]));
        assert_eq!(request.using.as_deref(), Some("run"));
    }
    let count = fixture.job_requests().len();
    for command in ["sh", "codex --version", "claude;id", ""] {
        assert!(
            handle
                .start_native_agent_version_probe(
                    &matched,
                    "/workspace",
                    command,
                    NativeVersionProbeOptions::default()
                )
                .await
                .is_err()
        );
    }
    assert_eq!(fixture.job_requests().len(), count);
}
