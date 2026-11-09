use std::{collections::HashSet, path::PathBuf, time::Duration};

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
        coding_agent::{HostWorkerContext, NativeInteractiveWorkerSpec, NativeWorkspaceBinding},
    },
};
use prost::Message;
use serde::Serialize as _;
use serde_json::json;

#[path = "fixtures/native_interactive_rpc.rs"]
mod native_interactive_rpc;

use native_interactive_rpc::{RpcFixture, TestServer, start_server};

fn spec() -> NativeInteractiveWorkerSpec {
    NativeInteractiveWorkerSpec {
        native_profile_id: "opencode-native".into(),
        repository_id: RepositoryId {
            host: "forge.example".into(),
            owner: "team".into(),
            name: "repo".into(),
        },
        use_static: true,
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

async fn registered(
    fixture: RpcFixture,
) -> (
    TestServer,
    JobworkerpHandle,
    lookback_tauri_lib::jobworkerp::coding_agent::NativeWorkerMatch,
) {
    let server = start_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let matched = handle
        .ensure_native_interactive_worker(spec())
        .await
        .unwrap();
    (server, handle, matched)
}

async fn start(
    handle: &JobworkerpHandle,
    matched: &lookback_tauri_lib::jobworkerp::coding_agent::NativeWorkerMatch,
) -> bool {
    handle
        .start_native_interactive_session(matched, "opencode", &[], "/workspace")
        .await
        .is_ok()
}

#[tokio::test]
async fn a_matched_native_session_uses_unbounded_job_deadline_and_literal_sandbox_arguments() {
    let fixture = RpcFixture::default();
    fixture.set_job_result_items(vec![
        ResultOutputItem {
            item: Some(Item::Data(
                SandboxExecResult {
                    result: Some(SandboxResult::Output(SandboxExecOutput {
                        stream: "stdout".into(),
                        data: vec![0, 0xff, b'o', b'k'],
                    })),
                }
                .encode_to_vec(),
            )),
        },
        ResultOutputItem {
            item: Some(Item::Data(
                SandboxExecResult {
                    result: Some(SandboxResult::Exit(SandboxExecExit {
                        exit_code: 73,
                        execution_time_ms: 4_200,
                        sandbox_id: "sandbox-live".into(),
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

    let args = vec!["--model".to_owned(), "trusted-profile-model".to_owned()];
    let mut session = handle
        .start_native_interactive_session(&matched, "opencode", &args, "/workspace/project")
        .await
        .unwrap();
    assert_eq!(session.job_id(), &JobId { value: 9_004 });

    let request = fixture.job_requests().pop().expect("one job was enqueued");
    assert_eq!(
        request.worker,
        Some(job_request::Worker::WorkerId(WorkerId {
            value: matched.worker_id().value,
        }))
    );
    assert_eq!(request.using.as_deref(), Some("run_with_client"));
    assert_eq!(request.timeout, Some(0));
    assert!(request.uniq_key.is_none());
    assert!(request.overrides.is_none());
    assert_eq!(fixture.grpc_timeout_headers(), vec![None]);

    let runner = fixture.runner_data();
    let descriptor =
        JobworkerpProto::parse_job_args_schema_descriptor(&runner, Some("run_with_client"))
            .unwrap()
            .unwrap();
    let decoded = prost_reflect::DynamicMessage::decode(descriptor, request.args.as_slice())
        .unwrap()
        .serialize(serde_json::value::Serializer)
        .unwrap();
    assert_eq!(decoded["command"], "opencode");
    assert_eq!(decoded["args"], json!(args));
    assert_eq!(decoded["workingDir"], "/workspace/project");
    let fields = decoded
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    assert_eq!(
        fields,
        HashSet::from(["command".into(), "args".into(), "workingDir".into()])
    );

    let input = session
        .input_handle()
        .expect("run_with_client accepts stdin");
    let input_bytes = vec![0, 0xff, b'x'];
    input.send_input(input_bytes.clone()).await.unwrap();
    input.finish_input().await.unwrap();
    let frames = tokio::time::timeout(Duration::from_secs(1), fixture.wait_for_final_feed_frame())
        .await
        .expect("fixture received stdin EOF");
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].data, input_bytes);
    assert!(!frames[0].is_final);
    assert!(frames[1].is_final);
    assert!(frames[1].data.is_empty());

    assert_eq!(
        session.next_event().await.unwrap(),
        Some(
            lookback_tauri_lib::jobworkerp::coding_agent::CodingAgentEvent::Output {
                stream: "stdout".into(),
                data: vec![0, 0xff, b'o', b'k'],
            }
        )
    );
    assert_eq!(
        session.next_event().await.unwrap(),
        Some(
            lookback_tauri_lib::jobworkerp::coding_agent::CodingAgentEvent::Exit {
                exit_code: 73,
                execution_time_ms: 4_200,
                sandbox_id: "sandbox-live".into(),
            }
        )
    );
    assert_eq!(session.next_event().await.unwrap(), None);
    drop(session);
    assert_eq!(fixture.counts().create_calls, 1);
    assert_eq!(fixture.counts().job_enqueue_calls, 1);
    assert_eq!(fixture.counts().job_delete_calls, 0);
    assert_eq!(fixture.counts().worker_delete_calls, 0);
    assert_eq!(fixture.counts().release_static_calls, 0);
}

#[tokio::test]
async fn session_cwd_must_be_normalized_and_component_contained_by_the_fixed_guest_root() {
    let fixture = RpcFixture::default();
    let (_server, handle, matched) = registered(fixture.clone()).await;

    for cwd in [
        "workspace/project",
        "/workspace/../outside",
        "/workspace-other/project",
        "/workspace//project",
        "/workspace/project/",
    ] {
        assert!(
            handle
                .start_native_interactive_session(&matched, "opencode", &[], cwd)
                .await
                .is_err(),
            "invalid guest cwd should be rejected: {cwd}"
        );
    }
    assert!(fixture.job_requests().is_empty());
    assert_eq!(fixture.counts().job_enqueue_calls, 0);
}

#[tokio::test]
async fn native_sessions_reuse_the_matched_worker_without_per_session_creation() {
    let fixture = RpcFixture::default();
    let (_server, handle, matched) = registered(fixture.clone()).await;

    let first = handle
        .start_native_interactive_session(&matched, "opencode", &[], "/workspace/first")
        .await
        .unwrap();
    let second = handle
        .start_native_interactive_session(&matched, "opencode", &[], "/workspace/second")
        .await
        .unwrap();

    assert_eq!(fixture.counts().create_calls, 1);
    assert_eq!(fixture.counts().job_enqueue_calls, 2);
    assert_eq!(first.job_id(), &JobId { value: 9_004 });
    assert_eq!(second.job_id(), &JobId { value: 9_005 });
}

#[tokio::test]
async fn oversized_literal_argv_is_rejected_before_job_enqueue() {
    let fixture = RpcFixture::default();
    let (_server, handle, matched) = registered(fixture.clone()).await;
    let huge_arg = "x".repeat(64 * 1024);

    assert!(
        handle
            .start_native_interactive_session(&matched, "opencode", &[huge_arg], "/workspace")
            .await
            .is_err()
    );
    assert_eq!(fixture.counts().job_enqueue_calls, 0);
}

#[tokio::test]
async fn worker_settings_drift_after_match_is_rejected_before_enqueue() {
    let fixture = RpcFixture::default();
    let (_server, handle, matched) = registered(fixture.clone()).await;
    let mut drifted = spec().runner_settings_json;
    drifted["vm"]["memory_mib"] = json!(3072);
    let descriptor =
        JobworkerpProto::parse_runner_settings_schema_descriptor(&fixture.runner_data())
            .unwrap()
            .unwrap();
    let bytes = JobworkerpProto::json_value_to_message(descriptor, &drifted, true, false).unwrap();
    fixture.mutate_first_worker(|worker| worker.runner_settings = bytes);

    assert!(!start(&handle, &matched).await);
    assert_eq!(fixture.counts().job_enqueue_calls, 0);
    assert!(fixture.job_requests().is_empty());
}

#[tokio::test]
async fn worker_policy_drift_after_match_is_rejected_before_enqueue() {
    let fixture = RpcFixture::default();
    let (_server, handle, matched) = registered(fixture.clone()).await;
    fixture.mutate_first_worker(|worker| worker.use_static = false);

    assert!(!start(&handle, &matched).await);
    assert_eq!(fixture.counts().job_enqueue_calls, 0);
    assert!(fixture.job_requests().is_empty());
}

#[tokio::test]
async fn a_replaced_worker_id_is_rejected_before_enqueue() {
    let fixture = RpcFixture::default();
    let (_server, handle, matched) = registered(fixture.clone()).await;
    fixture.replace_first_worker_id(WorkerId { value: 81 });

    assert!(!start(&handle, &matched).await);
    assert_eq!(matched.worker_id().value, 80);
    assert_eq!(fixture.counts().job_enqueue_calls, 0);
}

#[tokio::test]
async fn worker_lookup_errors_and_incomplete_replies_are_rejected_before_enqueue() {
    for malformed in [false, true] {
        let fixture = RpcFixture::default();
        let (_server, handle, matched) = registered(fixture.clone()).await;
        if malformed {
            fixture.malform_next_worker_lookup();
        } else {
            fixture.fail_next_worker_lookup_with_unknown();
        }

        assert!(!start(&handle, &matched).await);
        assert_eq!(fixture.counts().job_enqueue_calls, 0);
        assert!(fixture.job_requests().is_empty());
    }
}

#[tokio::test]
async fn runner_lookup_errors_and_incomplete_replies_are_rejected_before_enqueue() {
    for malformed in [false, true] {
        let fixture = RpcFixture::default();
        let (_server, handle, matched) = registered(fixture.clone()).await;
        if malformed {
            fixture.malform_next_runner_lookup();
        } else {
            fixture.fail_next_runner_lookup_with_unknown();
        }

        assert!(!start(&handle, &matched).await);
        assert_eq!(fixture.counts().job_enqueue_calls, 0);
        assert!(fixture.job_requests().is_empty());
    }
}

#[tokio::test]
async fn live_run_with_client_schema_drift_is_rejected_before_enqueue() {
    for drift in ["description", "result_schema"] {
        let fixture = RpcFixture::default();
        let (_server, handle, matched) = registered(fixture.clone()).await;
        fixture.mutate_runner_data(|runner| {
            let method = runner
                .method_proto_map
                .as_mut()
                .unwrap()
                .schemas
                .get_mut("run_with_client")
                .unwrap();
            if drift == "description" {
                method.description = Some("different live method".to_owned());
            } else {
                method.result_proto = r#"
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
                "#
                .to_owned();
            }
        });

        assert!(!start(&handle, &matched).await);
        assert_eq!(fixture.counts().job_enqueue_calls, 0);
        assert!(fixture.job_requests().is_empty());
    }
}

#[tokio::test]
async fn enqueue_failures_and_missing_or_invalid_job_ids_are_not_retried() {
    for behavior in ["unavailable", "missing_job_id", "invalid_job_id"] {
        let fixture = RpcFixture::default();
        match behavior {
            "unavailable" => fixture.set_enqueue_unavailable(),
            "missing_job_id" => fixture.omit_next_job_id(),
            "invalid_job_id" => fixture.return_invalid_next_job_id(),
            _ => unreachable!(),
        }
        let (_server, handle, matched) = registered(fixture.clone()).await;

        assert!(!start(&handle, &matched).await);
        assert_eq!(fixture.counts().job_enqueue_calls, 1);
        assert_eq!(fixture.job_requests().len(), 1);
    }
}
