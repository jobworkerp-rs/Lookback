use std::path::PathBuf;

use coding_agent_executor::RepositoryId;
use jobworkerp_client::{
    jobworkerp::data::{RetryType, RunnerId},
    proto::JobworkerpProto,
};
use lookback_tauri_lib::jobworkerp::{
    JobworkerpHandle,
    coding_agent::{NativeInteractiveWorkerSpec, NativeWorkspaceBinding},
};
use serde::Serialize as _;
use serde_json::json;

#[path = "fixtures/native_interactive_rpc.rs"]
mod native_interactive_rpc;

use native_interactive_rpc::{RpcFixture, start_server};

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
        host_context: lookback_tauri_lib::jobworkerp::coding_agent::HostWorkerContext {
            microsandbox_sdk_home: PathBuf::from("/home/test/.local/share/lookback/msb"),
            backend_config_path: PathBuf::from("/home/test/.local/share/lookback/msb/config.json"),
        },
    }
}

fn read_varint(bytes: &[u8], offset: &mut usize) -> u64 {
    let mut value = 0;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*offset).expect("protobuf varint is complete");
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
        assert!(shift < 64, "protobuf varint fits in 64 bits");
    }
}

fn length_delimited_field(bytes: &[u8], wanted_field: u64) -> Vec<u8> {
    let mut offset = 0;
    while offset < bytes.len() {
        let start = offset;
        let key = read_varint(bytes, &mut offset);
        let field_number = key >> 3;
        let wire_type = key & 7;
        match wire_type {
            0 => {
                read_varint(bytes, &mut offset);
            }
            1 => offset += 8,
            2 => {
                let payload_len = usize::try_from(read_varint(bytes, &mut offset))
                    .expect("protobuf field length fits in usize");
                offset += payload_len;
            }
            5 => offset += 4,
            _ => panic!("unsupported protobuf wire type {wire_type}"),
        }
        assert!(offset <= bytes.len(), "protobuf field is complete");
        if field_number == wanted_field && wire_type == 2 {
            return bytes[start..offset].to_vec();
        }
    }
    panic!("protobuf length-delimited field {wanted_field} exists");
}

#[tokio::test]
async fn one_shot_registration_does_not_require_interactive_schema_or_interactive_deadline_policy()
{
    let fixture = RpcFixture::default();
    fixture.mutate_runner_data(|runner| {
        runner
            .method_proto_map
            .as_mut()
            .unwrap()
            .schemas
            .remove("run_with_client");
    });
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut configured = spec();
    configured.runner_settings_json["vm"]["max_duration_sec"] = json!(120);
    configured.runner_settings_json["vm"]["idle_timeout_sec"] = json!(60);
    assert!(
        handle
            .ensure_native_one_shot_worker(configured.clone())
            .await
            .is_ok()
    );
    assert!(
        handle
            .ensure_native_interactive_worker(configured)
            .await
            .is_err()
    );
    assert_eq!(fixture.created_workers().len(), 1);
    assert_eq!(fixture.counts().enqueue_for_stream_calls, 0);
}

#[tokio::test]
async fn automatic_and_interactive_routes_keep_separate_workers_and_preserve_static_policy() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    for use_static in [true, false] {
        let mut configured = spec();
        configured.use_static = use_static;
        let automatic = handle
            .ensure_native_one_shot_worker(configured.clone())
            .await
            .unwrap();
        let reused = handle
            .ensure_native_one_shot_worker(configured.clone())
            .await
            .unwrap();
        let interactive = handle
            .ensure_native_interactive_worker(configured)
            .await
            .unwrap();
        assert_eq!(automatic.worker_id(), reused.worker_id());
        assert_ne!(automatic.worker_id(), interactive.worker_id());
        assert_eq!(automatic.using(), "run");
        assert!(!automatic.method_schema().require_client_stream);
        assert_eq!(interactive.using(), "run_with_client");
        assert!(
            automatic
                .worker_name()
                .starts_with("lookback.coding-agent.one-shot.")
        );
        let worker = fixture
            .created_workers()
            .into_iter()
            .find(|worker| worker.name == automatic.worker_name())
            .unwrap();
        assert_eq!(worker.use_static, use_static);
        assert_eq!(worker.channel.as_deref(), Some("coding-agent-one-shot"));
        assert!(
            handle
                .start_native_interactive_session(
                    &automatic,
                    "opencode",
                    &["--prompt=prompt".into()],
                    "/workspace/run"
                )
                .await
                .is_err()
        );
    }
    assert_eq!(fixture.created_workers().len(), 4);
    assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 0);
    assert_eq!(fixture.counts().enqueue_for_stream_calls, 0);
}

#[tokio::test]
async fn a_native_profile_and_repository_reuse_one_static_worker_with_schema_encoded_settings() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();

    let first = handle
        .ensure_native_interactive_worker(spec())
        .await
        .unwrap();
    let second = handle
        .ensure_native_interactive_worker(spec())
        .await
        .unwrap();

    assert_eq!(first.worker_id(), second.worker_id());
    assert_eq!(first.settings_digest(), second.settings_digest());
    assert_eq!(first.using(), "run_with_client");

    let created = fixture.created_workers();
    assert_eq!(
        created.len(),
        1,
        "a fixed profile/repository owns one worker"
    );
    let worker = &created[0];
    let name_digest = worker
        .name
        .strip_prefix("lookback.interactive.")
        .expect("native worker names use the stable namespace");
    assert_eq!(name_digest.len(), 32, "worker names use a digest prefix");
    assert_eq!(worker.runner_id, Some(RunnerId { value: 17 }));
    assert_eq!(
        worker.retry_policy.as_ref().unwrap().r#type,
        RetryType::None as i32
    );
    assert!(
        worker.use_static,
        "interactive sessions reuse the static runner pool"
    );
    assert_eq!(
        fixture.counts().upsert_calls,
        0,
        "worker creation must not overwrite by name"
    );

    let descriptor =
        JobworkerpProto::parse_runner_settings_schema_descriptor(&fixture.runner_data())
            .unwrap()
            .unwrap();
    let dynamic =
        prost_reflect::DynamicMessage::decode(descriptor, worker.runner_settings.as_slice())
            .unwrap();
    let decoded = dynamic.serialize(serde_json::value::Serializer).unwrap();
    assert_eq!(
        decoded["vm"]["image"],
        spec().runner_settings_json["vm"]["image"]
    );
    assert_eq!(decoded["vm"]["rootDiskMib"], 8192);
    assert_eq!(decoded["vm"]["mounts"][0]["guestPath"], "/workspace");
    assert_eq!(decoded["vm"]["mounts"][0]["writable"], true);
    assert!(decoded.get("microsandbox_sdk_home").is_none());
    assert!(decoded.get("backend_config_path").is_none());
}

#[tokio::test]
async fn worker_identity_changes_for_profile_repository_or_fixed_settings_not_session_input() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();

    let base = spec();
    let same = handle
        .ensure_native_interactive_worker(base.clone())
        .await
        .unwrap();

    let mut other_profile = base.clone();
    other_profile.native_profile_id = "other-profile".into();
    let profile_match = handle
        .ensure_native_interactive_worker(other_profile)
        .await
        .unwrap();

    let mut other_repository = base.clone();
    other_repository.repository_id.name = "other-repo".into();
    let repository_match = handle
        .ensure_native_interactive_worker(other_repository)
        .await
        .unwrap();

    let mut other_settings = base.clone();
    other_settings.runner_settings_json["vm"]["memory_mib"] = json!(3072);
    let settings_match = handle
        .ensure_native_interactive_worker(other_settings)
        .await
        .unwrap();

    let mut other_environment = base.clone();
    other_environment.runner_settings_json["vm"]["env"]["CARGO_HOME"] =
        json!("/home/codespace/.cargo");
    let environment_match = handle
        .ensure_native_interactive_worker(other_environment)
        .await
        .unwrap();

    let identities = [
        same.settings_digest(),
        profile_match.settings_digest(),
        repository_match.settings_digest(),
        settings_match.settings_digest(),
        environment_match.settings_digest(),
    ];
    for left in 0..identities.len() {
        for right in left + 1..identities.len() {
            assert_ne!(identities[left], identities[right]);
        }
    }
    assert_eq!(fixture.created_workers().len(), identities.len());
}

#[tokio::test]
async fn profile_static_policy_is_preserved_and_part_of_worker_identity() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut dynamic_spec = spec();
    dynamic_spec.use_static = false;

    let first = handle
        .ensure_native_interactive_worker(dynamic_spec.clone())
        .await
        .unwrap();
    let second = handle
        .ensure_native_interactive_worker(dynamic_spec.clone())
        .await
        .unwrap();
    assert_eq!(first.worker_id(), second.worker_id());
    assert_eq!(first.settings_digest(), second.settings_digest());
    assert_eq!(fixture.created_workers().len(), 1);
    assert!(!fixture.created_workers()[0].use_static);

    dynamic_spec.use_static = true;
    let static_match = handle
        .ensure_native_interactive_worker(dynamic_spec)
        .await
        .unwrap();
    assert_ne!(first.settings_digest(), static_match.settings_digest());
    assert_ne!(first.worker_id(), static_match.worker_id());
    assert_eq!(fixture.created_workers().len(), 2);
    assert!(fixture.created_workers()[1].use_static);
    assert_ne!(
        fixture.created_workers()[0].name,
        fixture.created_workers()[1].name
    );
}

#[tokio::test]
async fn a_same_name_worker_with_different_policy_is_rejected_instead_of_updated() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    handle
        .ensure_native_interactive_worker(spec())
        .await
        .unwrap();

    fixture.mutate_first_worker(|worker| worker.use_static = false);

    assert!(
        handle
            .ensure_native_interactive_worker(spec())
            .await
            .is_err()
    );
    assert_eq!(fixture.created_workers().len(), 1);
    assert_eq!(fixture.counts().upsert_calls, 0);
}

#[tokio::test]
async fn a_create_race_reuses_the_matching_worker_returned_by_name_lookup() {
    let fixture = RpcFixture::default();
    fixture.simulate_create_race();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();

    let worker = handle
        .ensure_native_interactive_worker(spec())
        .await
        .unwrap();

    assert_eq!(worker.worker_id().value, 80);
    assert_eq!(fixture.created_workers().len(), 1);
    assert_eq!(fixture.counts().create_calls, 1);
    assert_eq!(fixture.counts().worker_lookups, 2);
    assert_eq!(fixture.counts().upsert_calls, 0);
}

#[tokio::test]
async fn an_already_exists_create_without_a_visible_worker_is_rejected() {
    let fixture = RpcFixture::default();
    fixture.simulate_unavailable_worker_after_create_race();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();

    assert!(
        handle
            .ensure_native_interactive_worker(spec())
            .await
            .is_err()
    );
    assert_eq!(fixture.counts().create_calls, 1);
    assert_eq!(fixture.counts().worker_lookups, 2);
    assert!(fixture.created_workers().is_empty());
    assert_eq!(fixture.counts().upsert_calls, 0);
}

#[tokio::test]
async fn an_unknown_worker_lookup_error_is_not_treated_as_worker_absence() {
    let fixture = RpcFixture::default();
    fixture.fail_next_worker_lookup_with_unknown();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();

    assert!(
        handle
            .ensure_native_interactive_worker(spec())
            .await
            .is_err()
    );
    assert_eq!(fixture.counts().worker_lookups, 1);
    assert_eq!(fixture.counts().create_calls, 0);
    assert!(fixture.created_workers().is_empty());
}

#[tokio::test]
async fn an_incomplete_worker_lookup_reply_is_rejected_without_creation() {
    let fixture = RpcFixture::default();
    fixture.malform_next_worker_lookup();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();

    assert!(
        handle
            .ensure_native_interactive_worker(spec())
            .await
            .is_err()
    );
    assert_eq!(fixture.counts().worker_lookups, 1);
    assert_eq!(fixture.counts().create_calls, 0);
    assert!(fixture.created_workers().is_empty());
}

#[tokio::test]
async fn a_same_identity_worker_with_valid_but_drifted_runner_settings_is_rejected() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    handle
        .ensure_native_interactive_worker(spec())
        .await
        .unwrap();

    let mut drifted_settings = spec();
    drifted_settings.runner_settings_json["vm"]["memory_mib"] = json!(3072);
    let descriptor =
        JobworkerpProto::parse_runner_settings_schema_descriptor(&fixture.runner_data())
            .unwrap()
            .unwrap();
    let drifted_bytes = JobworkerpProto::json_value_to_message(
        descriptor,
        &drifted_settings.runner_settings_json,
        true,
        false,
    )
    .unwrap();
    fixture.mutate_first_worker(|worker| worker.runner_settings = drifted_bytes);

    assert!(
        handle
            .ensure_native_interactive_worker(spec())
            .await
            .is_err()
    );
    assert_eq!(fixture.created_workers().len(), 1);
    assert_eq!(fixture.counts().create_calls, 1);
    assert_eq!(fixture.counts().upsert_calls, 0);
}

#[tokio::test]
async fn equivalent_protobuf_settings_are_reused_even_when_the_wire_bytes_differ() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let first = handle
        .ensure_native_interactive_worker(spec())
        .await
        .unwrap();
    let original_bytes = fixture.first_worker_data().runner_settings;

    // Duplicate the same network submessage, whose only configured value is
    // optional enabled=false. Protobuf merge semantics preserve its meaning.
    fixture.mutate_first_worker(|worker| {
        let network_field = length_delimited_field(&worker.runner_settings, 2);
        worker.runner_settings.extend(network_field);
    });
    assert_ne!(
        fixture.first_worker_data().runner_settings,
        original_bytes,
        "the fixture must exercise semantic rather than byte comparison"
    );

    let second = handle
        .ensure_native_interactive_worker(spec())
        .await
        .unwrap();
    assert_eq!(first.worker_id(), second.worker_id());
    assert_eq!(fixture.created_workers().len(), 1);
}

#[tokio::test]
async fn settings_must_match_the_live_runner_schema_without_unknown_fields() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut invalid = spec();
    invalid.runner_settings_json["unregistered_override"] = json!(true);

    assert!(
        handle
            .ensure_native_interactive_worker(invalid)
            .await
            .is_err()
    );
    assert!(fixture.created_workers().is_empty());
    assert_eq!(fixture.counts().upsert_calls, 0);
}

#[tokio::test]
async fn interactive_worker_settings_must_leave_all_execution_deadlines_unset() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();

    for (path, value) in [
        ("vm.max_duration_sec", json!(0)),
        ("vm.idle_timeout_sec", json!(60)),
        ("default_exec_timeout_ms", json!(0)),
    ] {
        let mut invalid = spec();
        match path {
            "vm.max_duration_sec" => invalid.runner_settings_json["vm"]["max_duration_sec"] = value,
            "vm.idle_timeout_sec" => invalid.runner_settings_json["vm"]["idle_timeout_sec"] = value,
            "default_exec_timeout_ms" => {
                invalid.runner_settings_json["default_exec_timeout_ms"] = value;
            }
            _ => unreachable!(),
        }

        assert!(
            handle
                .ensure_native_interactive_worker(invalid)
                .await
                .is_err(),
            "fixed interactive settings must reject {path}"
        );
    }

    assert!(fixture.created_workers().is_empty());
    assert_eq!(fixture.counts().create_calls, 0);
}

#[tokio::test]
async fn unsupported_interactive_method_schema_fails_before_worker_creation() {
    let mut runner = RpcFixture::default().runner_data();
    runner.method_proto_map = None;
    let fixture = RpcFixture::with_runner_data(runner);
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();

    assert!(
        handle
            .ensure_native_interactive_worker(spec())
            .await
            .is_err()
    );
    assert!(fixture.created_workers().is_empty());
}

#[tokio::test]
async fn invalid_host_context_is_rejected_before_contacting_jobworkerp() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut invalid = spec();
    invalid.host_context.microsandbox_sdk_home = PathBuf::from("relative/sdk-home");

    assert!(
        handle
            .ensure_native_interactive_worker(invalid)
            .await
            .is_err()
    );
    assert_eq!(fixture.counts().runner_lookups, 0);
    assert!(fixture.created_workers().is_empty());
}

#[tokio::test]
async fn a_workspace_mount_must_match_the_fixed_repository_root_and_guest_path() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut invalid = spec();
    invalid.runner_settings_json["vm"]["mounts"][0]["guest_path"] = json!("/other");

    assert!(
        handle
            .ensure_native_interactive_worker(invalid)
            .await
            .is_err()
    );
    assert!(fixture.created_workers().is_empty());
    assert_eq!(fixture.counts().upsert_calls, 0);
}

#[tokio::test]
async fn workspace_mount_requires_explicit_writable_true() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();

    let mut read_only = spec();
    read_only.runner_settings_json["vm"]["mounts"][0]["writable"] = json!(false);
    let mut omitted = spec();
    omitted.runner_settings_json["vm"]["mounts"][0]
        .as_object_mut()
        .unwrap()
        .remove("writable");

    assert!(
        handle
            .ensure_native_interactive_worker(read_only)
            .await
            .is_err(),
        "an explicitly read-only workspace cannot run an interactive agent"
    );
    assert!(
        handle
            .ensure_native_interactive_worker(omitted)
            .await
            .is_err(),
        "protobuf bool default false must not be treated as writable"
    );
    assert!(fixture.created_workers().is_empty());
}

#[tokio::test]
async fn an_env_map_cannot_stand_in_for_a_vm_mount() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut decoy = spec();
    decoy.runner_settings_json["vm"]["mounts"] = json!([]);
    decoy.runner_settings_json["vm"]["env"] = json!({
        "hostPath": "/home/test/workspaces/team/repo",
        "guestPath": "/workspace",
        "writable": "true"
    });

    assert!(
        handle
            .ensure_native_interactive_worker(decoy)
            .await
            .is_err()
    );
    assert!(fixture.created_workers().is_empty());
}

#[tokio::test]
async fn a_mount_outside_the_fixed_repository_root_is_rejected() {
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut outside = spec();
    outside.runner_settings_json["vm"]["mounts"][0]["host_path"] =
        json!("/home/test/workspaces/team/repo-elsewhere");

    assert!(
        handle
            .ensure_native_interactive_worker(outside)
            .await
            .is_err()
    );
    assert!(fixture.created_workers().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_workspace_root_is_rejected_without_scanning_its_contents() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let real_root = directory.path().join("real-repository");
    std::fs::create_dir(&real_root).unwrap();
    let symlink_root = directory.path().join("repository-link");
    symlink(&real_root, &symlink_root).unwrap();

    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut linked = spec();
    linked.workspace_binding.canonical_host_root = symlink_root.clone();
    linked.runner_settings_json["vm"]["mounts"][0]["host_path"] =
        json!(symlink_root.to_str().unwrap());
    linked.runner_settings_json["allowed_host_mounts"][0]["host_root"] =
        json!(symlink_root.to_str().unwrap());

    assert!(
        handle
            .ensure_native_interactive_worker(linked)
            .await
            .is_err()
    );
    assert!(fixture.created_workers().is_empty());
}

#[tokio::test]
async fn one_shot_registration_returns_a_real_worker_without_enqueuing_the_agent() {
    use lookback_tauri_lib::coding_agent::one_shot_workflow::register_one_shot_workflow;
    let fixture = RpcFixture::default();
    fixture.enable_preparation_workflow();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let matched = handle.ensure_native_one_shot_worker(spec()).await.unwrap();
    let registered = register_one_shot_workflow(&handle, &matched).await.unwrap();
    assert!(registered.worker_id() > 0);
    assert_ne!(registered.worker_id(), matched.worker_id().value);
    assert_eq!(fixture.counts().job_enqueue_calls, 0);
    assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 0);
    let repeated = register_one_shot_workflow(&handle, &matched).await.unwrap();
    assert_eq!(registered.worker_id(), repeated.worker_id());
    assert_eq!(registered.worker_name(), repeated.worker_name());
    assert_eq!(fixture.counts().job_enqueue_calls, 0);
}

#[tokio::test]
async fn registered_one_shot_refuses_workflow_drift_without_enqueuing() {
    use lookback_tauri_lib::coding_agent::one_shot_workflow::register_one_shot_workflow;
    for mutation in 0..5 {
        let fixture = RpcFixture::default();
        fixture.enable_preparation_workflow();
        let server = start_server(fixture.clone()).await;
        let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
        let matched = handle.ensure_native_one_shot_worker(spec()).await.unwrap();
        let registered = register_one_shot_workflow(&handle, &matched).await.unwrap();
        fixture.mutate_named_worker(registered.worker_name(), |worker| {
            let data = worker.data.as_mut().unwrap();
            match mutation {
                0 => worker.id.as_mut().unwrap().value += 1,
                1 => data.runner_settings = vec![],
                2 => data.runner_id = Some(RunnerId { value: 17 }),
                3 => data.response_type = 0,
                _ => data.channel = Some("unapproved".into()),
            }
        });
        assert!(
            registered
                .start(
                    &handle,
                    &["opencode".into(), "run".into()],
                    "prompt",
                    "/workspace/run",
                    120
                )
                .await
                .is_err(),
            "drift {mutation} was accepted"
        );
        assert_eq!(fixture.counts().job_enqueue_calls, 0);
        assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 0);
    }
}

#[tokio::test]
async fn registered_one_shot_enqueues_the_verified_id_without_lookup_or_recreation() {
    use jobworkerp_client::jobworkerp::service::job_request;
    use lookback_tauri_lib::coding_agent::one_shot_workflow::register_one_shot_workflow;
    for remove in [false, true] {
        let fixture = RpcFixture::default();
        fixture.enable_preparation_workflow();
        let server = start_server(fixture.clone()).await;
        let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
        let matched = handle.ensure_native_one_shot_worker(spec()).await.unwrap();
        let registered = register_one_shot_workflow(&handle, &matched).await.unwrap();
        let before = fixture.counts();
        fixture.change_worker_after_workflow_runner_lookup(registered.worker_name(), remove);
        let _ = registered
            .start(
                &handle,
                &["opencode".into(), "run".into()],
                "prompt",
                "/workspace/run",
                120,
            )
            .await;
        let requests = fixture.job_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].worker,
            Some(job_request::Worker::WorkerId(
                jobworkerp_client::jobworkerp::data::WorkerId {
                    value: registered.worker_id()
                }
            ))
        );
        assert_eq!(fixture.counts().create_calls, before.create_calls);
        assert_eq!(fixture.counts().upsert_calls, before.upsert_calls);
    }
}

#[tokio::test]
async fn one_shot_acceptance_returns_the_actual_parent_job_before_waiting_for_output() {
    use jobworkerp_client::jobworkerp::data::{
        ResultOutputItem, Trailer, result_output_item::Item,
    };
    use lookback_tauri_lib::coding_agent::one_shot_workflow::start_one_shot_workflow;
    let fixture = RpcFixture::default();
    fixture.enable_preparation_workflow();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let matched = handle.ensure_native_one_shot_worker(spec()).await.unwrap();
    let descriptor = JobworkerpProto::parse_result_schema_descriptor(
        &fixture.workflow_runner_data(),
        Some("run"),
    )
    .unwrap()
    .unwrap();
    fixture.set_job_result_items(vec![ResultOutputItem { item: Some(Item::Data(JobworkerpProto::json_value_to_message(descriptor, &json!({"id":"actual-workflow-result", "status":"Completed", "output":json!({"exit":{"sandboxId":"actual-sandbox", "exitCode":0}}).to_string()}), true, false).unwrap())) }, ResultOutputItem { item: Some(Item::End(Trailer::default())) }]);
    let gate = fixture.block_next_output();
    let command = vec!["opencode".into(), "run".into()];
    let execution = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        start_one_shot_workflow(
            &handle,
            &matched,
            &command,
            "Issue prompt",
            "/workspace/run",
            120,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(execution.parent_job_id().is_some_and(|id| id > 0));
    gate.wait_until_entered().await;
    let parent_id = jobworkerp_client::jobworkerp::data::JobId {
        value: execution.parent_job_id().unwrap(),
    };
    assert!(handle.cancel(parent_id).await.is_err());
    assert_eq!(fixture.cancelled_job_ids(), vec![parent_id]);
    gate.resume();
    let finished = execution.finish().await.unwrap();
    assert_eq!(finished.exit_code, 0);
    assert_eq!(finished.parent_job_id, Some(parent_id.value));
    assert_eq!(
        finished.workflow_execution_id.as_deref(),
        Some("actual-workflow-result")
    );
    fixture.omit_next_job_id();
    let without_job_id = start_one_shot_workflow(
        &handle,
        &matched,
        &command,
        "Issue prompt",
        "/workspace/run",
        120,
    )
    .await
    .unwrap();
    assert_eq!(without_job_id.parent_job_id(), None);
    let finished = without_job_id.finish().await.unwrap();
    assert_eq!(finished.exit_code, 0);
    assert_eq!(finished.parent_job_id, None);
    fixture.return_invalid_next_job_id();
    assert!(
        start_one_shot_workflow(
            &handle,
            &matched,
            &command,
            "Issue prompt",
            "/workspace/run",
            120
        )
        .await
        .is_err()
    );
    assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 0);
}

#[tokio::test]
async fn one_shot_dispatches_only_a_workflow_and_requires_completed_result_with_real_end() {
    use jobworkerp_client::jobworkerp::data::{
        ResultOutputItem, Trailer, result_output_item::Item,
    };
    use lookback_tauri_lib::coding_agent::one_shot_workflow::run_one_shot_workflow;
    let fixture = RpcFixture::default();
    fixture.enable_preparation_workflow();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let matched = handle.ensure_native_one_shot_worker(spec()).await.unwrap();
    let descriptor = JobworkerpProto::parse_result_schema_descriptor(
        &fixture.workflow_runner_data(),
        Some("run"),
    )
    .unwrap()
    .unwrap();
    let data = |status: &str, output: serde_json::Value| ResultOutputItem {
        item: Some(Item::Data(
            JobworkerpProto::json_value_to_message(
                descriptor.clone(),
                &json!({
                    "id":"actual-workflow-result", "status":status, "output":output.to_string(),
                }),
                true,
                false,
            )
            .unwrap(),
        )),
    };
    let end = || ResultOutputItem {
        item: Some(Item::End(Trailer::default())),
    };
    let exit =
        json!({"exit":{"exitCode":0,"sandboxId":"observed-sandbox","executionTimeMs":"123"}});
    fixture.set_job_result_items(vec![data("Completed", exit.clone()), end()]);
    let command = vec!["opencode".into(), "run".into()];
    let prompt = "--help\n日本語 $(never)";
    assert_eq!(
        run_one_shot_workflow(&handle, &matched, &command, prompt, "/workspace/run", 120)
            .await
            .unwrap()
            .exit_code,
        0
    );
    let workflow_worker = fixture
        .created_workers()
        .into_iter()
        .find(|worker| {
            worker.name.starts_with("lookback.coding-agent.one-shot.")
                && worker.runner_id != Some(RunnerId { value: 17 })
        })
        .unwrap();
    let settings = prost_reflect::DynamicMessage::decode(
        JobworkerpProto::parse_runner_settings_schema_descriptor(&fixture.workflow_runner_data())
            .unwrap()
            .unwrap(),
        workflow_worker.runner_settings.as_slice(),
    )
    .unwrap()
    .serialize(serde_json::value::Serializer)
    .unwrap();
    let definition: serde_json::Value =
        serde_json::from_str(settings["workflowData"].as_str().unwrap()).unwrap();
    let function = &definition["do"][0]["runAgent"]["run"]["function"];
    assert_eq!(function["workerName"], matched.worker_name());
    assert_eq!(function["using"], "run");
    assert_eq!(
        function["arguments"]["args"],
        "${ $workflow.input.arguments.args }"
    );
    let request = fixture.job_requests().pop().unwrap();
    let args = prost_reflect::DynamicMessage::decode(
        JobworkerpProto::parse_job_args_schema_descriptor(
            &fixture.workflow_runner_data(),
            Some("run"),
        )
        .unwrap()
        .unwrap(),
        request.args.as_slice(),
    )
    .unwrap()
    .serialize(serde_json::value::Serializer)
    .unwrap();
    let input: serde_json::Value = serde_json::from_str(args["input"].as_str().unwrap()).unwrap();
    assert_eq!(
        input["arguments"]["args"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        prompt
    );
    assert!(!input["arguments"]["tty"].as_bool().unwrap());
    assert!(!definition.to_string().contains(prompt));
    let worker_names = || {
        fixture
            .created_workers()
            .into_iter()
            .map(|worker| worker.name)
            .collect::<std::collections::BTreeSet<_>>()
    };
    let initial_names = worker_names();
    run_one_shot_workflow(
        &handle,
        &matched,
        &command,
        "別のprompt",
        "/workspace/other-run",
        180,
    )
    .await
    .unwrap();
    assert_eq!(worker_names(), initial_names);
    for items in [
        vec![data("Completed", exit.clone())],
        vec![data("Faulted", exit.clone()), end()],
        vec![
            data(
                "Completed",
                json!({"output":{"stream":"stdout","data":"eA=="}}),
            ),
            end(),
        ],
    ] {
        fixture.set_job_result_items(items);
        assert!(
            run_one_shot_workflow(&handle, &matched, &command, prompt, "/workspace/run", 120)
                .await
                .is_err()
        );
    }
    fixture.set_job_result_items(vec![data("Completed", exit), end()]);
    fixture.fail_stream_after_output();
    assert!(
        run_one_shot_workflow(&handle, &matched, &command, prompt, "/workspace/run", 120)
            .await
            .is_err()
    );
    assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 0);
    assert_eq!(fixture.counts().enqueue_for_stream_calls, 6);
    assert!(
        fixture
            .job_requests()
            .iter()
            .all(|request| request.using.as_deref() == Some("run"))
    );
}

#[tokio::test]
async fn automatic_runtime_checks_the_actual_cli_before_registering_the_execution_workflow() {
    use jobworkerp_client::jobworkerp::data::{
        ResultOutputItem, Trailer, result_output_item::Item,
    };
    use lookback_tauri_lib::coding_agent::one_shot_workflow::verify_one_shot_runtime;
    use lookback_tauri_lib::native_terminal::launch::{NativeAgentKind, NativeCliProfile};
    for failure in 0..9 {
        let fixture = RpcFixture::default();
        fixture.enable_preparation_workflow();
        let server = start_server(fixture.clone()).await;
        let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
        let matched = handle.ensure_native_one_shot_worker(spec()).await.unwrap();
        let descriptor =
            JobworkerpProto::parse_result_schema_descriptor(&fixture.runner_data(), Some("run"))
                .unwrap()
                .unwrap();
        let data = |value| ResultOutputItem {
            item: Some(Item::Data(
                JobworkerpProto::json_value_to_message(descriptor.clone(), &value, true, false)
                    .unwrap(),
            )),
        };
        let version = match failure {
            1 => "MS4xOC4zCg==",
            7 => "Y29kZXgtY2xpIDAuMTYwLjEK",
            8 => "Y29kZXggMC4xNjAuMQo=",
            _ => "Mi4wLjI0Cg==",
        };
        let stream = if failure == 2 { "stderr" } else { "stdout" };
        let mut items = vec![data(json!({"output":{"stream":stream,"data":version}}))];
        if matches!(failure, 6 | 7) {
            items.push(data(
                json!({"output":{"stream":"stderr","data":"d2FybmluZwo="}}),
            ));
        }
        if failure != 4 {
            items.push(data(json!({"exit":{"exitCode":if failure == 3 {1} else {0},"sandboxId":"probe-sandbox"}})));
        }
        if failure != 5 {
            items.push(ResultOutputItem {
                item: Some(Item::End(Trailer::default())),
            });
        }
        fixture.set_job_result_items(items);
        let profile = NativeCliProfile {
            kind: if failure >= 7 {
                NativeAgentKind::Codex
            } else {
                NativeAgentKind::OpenCode
            },
            cli_version: if failure >= 7 { "0.160.1" } else { "2.0.24" }.into(),
            runtime_image_digest: matched.scope().image_digest().into(),
            expected_profile_command: if failure >= 7 {
                vec!["codex".into(), "exec".into()]
            } else {
                vec!["opencode".into(), "run".into()]
            },
        };
        let checked =
            verify_one_shot_runtime(&handle, &matched, &profile, "/workspace/runs/prepared").await;
        assert_eq!(
            checked.is_ok(),
            matches!(failure, 0 | 1 | 6 | 7),
            "failure case {failure}"
        );
        assert_eq!(
            fixture.counts().job_enqueue_calls,
            1,
            "only the version probe may run"
        );
        assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 0);
        if let Ok(checked) = checked {
            let expected_version = match failure {
                1 => "1.18.3",
                7 => "0.160.1",
                _ => "2.0.24",
            };
            assert_eq!(checked.cli_version(), expected_version);
            assert!(checked.probe_job_id() > 0);
            assert!(checked.workflow().worker_id() > 0);
        } else {
            assert_eq!(
                fixture.counts().upsert_calls,
                0,
                "invalid CLI must not register an execution workflow"
            );
        }
    }
}

#[tokio::test]
async fn registering_an_automatic_workflow_rejects_unusable_live_method_schemas_before_approval() {
    use lookback_tauri_lib::coding_agent::one_shot_workflow::register_one_shot_workflow;
    for field in ["args", "result"] {
        let fixture = RpcFixture::default();
        fixture.enable_preparation_workflow();
        fixture.mutate_workflow_runner(|runner| {
            let method = runner
                .method_proto_map
                .as_mut()
                .unwrap()
                .schemas
                .get_mut("run")
                .unwrap();
            if field == "args" {
                method.args_proto = "not a proto".into();
            } else {
                method.result_proto = "not a proto".into();
            }
        });
        let server = start_server(fixture.clone()).await;
        let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
        let matched = handle.ensure_native_one_shot_worker(spec()).await.unwrap();
        assert!(register_one_shot_workflow(&handle, &matched).await.is_err());
        assert_eq!(fixture.counts().job_enqueue_calls, 0);
    }
}

#[tokio::test]
async fn automatic_runtime_refuses_worker_changes_after_the_live_cli_probe() {
    use jobworkerp_client::jobworkerp::data::{
        ResultOutputItem, Trailer, result_output_item::Item,
    };
    use lookback_tauri_lib::{
        coding_agent::one_shot_workflow::verify_one_shot_runtime,
        native_terminal::launch::{NativeAgentKind, NativeCliProfile},
    };
    let fixture = RpcFixture::default();
    fixture.enable_preparation_workflow();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let matched = handle.ensure_native_one_shot_worker(spec()).await.unwrap();
    let descriptor =
        JobworkerpProto::parse_result_schema_descriptor(&fixture.runner_data(), Some("run"))
            .unwrap()
            .unwrap();
    let data = |value| ResultOutputItem {
        item: Some(Item::Data(
            JobworkerpProto::json_value_to_message(descriptor.clone(), &value, true, false)
                .unwrap(),
        )),
    };
    fixture.set_job_result_items(vec![
        data(json!({"output":{"stream":"stdout","data":"Mi4wLjI0Cg=="}})),
        data(json!({"exit":{"exitCode":0,"sandboxId":"probe"}})),
        ResultOutputItem {
            item: Some(Item::End(Trailer::default())),
        },
    ]);
    let profile = NativeCliProfile {
        kind: NativeAgentKind::OpenCode,
        cli_version: "2.0.24".into(),
        runtime_image_digest: matched.scope().image_digest().into(),
        expected_profile_command: vec!["opencode".into(), "run".into()],
    };
    let gate = fixture.block_next_output();
    let worker_name = matched.worker_name().to_owned();
    let probe = tokio::spawn(async move {
        verify_one_shot_runtime(&handle, &matched, &profile, "/workspace/prepared").await
    });
    gate.wait_until_entered().await;
    fixture.mutate_named_worker(&worker_name, |worker| {
        worker.data.as_mut().unwrap().runner_settings.clear()
    });
    gate.resume();
    assert!(probe.await.unwrap().is_err());
    assert_eq!(fixture.counts().job_enqueue_calls, 1);
    assert_eq!(fixture.counts().upsert_calls, 0);
}

#[tokio::test]
async fn local_commit_is_a_registered_workflow_with_literal_branch_and_message() {
    use jobworkerp_client::jobworkerp::data::{
        ResultOutputItem, Trailer, result_output_item::Item,
    };
    use lookback_tauri_lib::coding_agent::workspace_commit::commit_workspace;
    let fixture = RpcFixture::default();
    fixture.enable_preparation_workflow();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let matched = handle.ensure_native_one_shot_worker(spec()).await.unwrap();
    let descriptor = JobworkerpProto::parse_result_schema_descriptor(
        &fixture.workflow_runner_data(),
        Some("run"),
    )
    .unwrap()
    .unwrap();
    for exit_code in [0, 1] {
        fixture.set_job_result_items(vec![ResultOutputItem{item:Some(Item::Data(JobworkerpProto::json_value_to_message(descriptor.clone(),
            &json!({"id":"actual-commit-workflow","status":"Completed","output":json!({"exit":{"exitCode":exit_code,"sandboxId":"commit-sandbox"}}).to_string()}),true,false).unwrap()))},
            ResultOutputItem{item:Some(Item::End(Trailer::default()))}]);
        let result = commit_workspace(
            &handle,
            &matched,
            "/workspace/run",
            "agent/issue-42-run",
            "Issue 日本語 $(never)",
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, exit_code);
        assert_eq!(
            result.workflow_execution_id.as_deref(),
            Some("actual-commit-workflow")
        );
        assert!(result.parent_job_id.is_some());
    }
    assert_eq!(fixture.counts().job_enqueue_calls, 2);
    assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 0);
    let worker = fixture
        .created_workers()
        .into_iter()
        .find(|worker| {
            worker
                .name
                .starts_with("lookback.coding-agent.local-commit.")
        })
        .unwrap();
    assert_ne!(worker.runner_id, Some(RunnerId { value: 17 }));
}

#[tokio::test]
async fn profile_settings_shell_uses_a_reusable_worker_without_an_issue_repository() {
    use lookback_tauri_lib::jobworkerp::coding_agent::NativeProfileSettingsWorkerSpec;
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let configured = spec();
    let settings = NativeProfileSettingsWorkerSpec {
        native_profile_id: configured.native_profile_id.clone(),
        use_static: configured.use_static,
        workspace_binding: configured.workspace_binding.clone(),
        runner_settings_json: configured.runner_settings_json.clone(),
        host_context: configured.host_context.clone(),
    };
    let first = handle
        .ensure_native_profile_settings_worker(settings.clone())
        .await
        .unwrap();
    let reused = handle
        .ensure_native_profile_settings_worker(settings)
        .await
        .unwrap();
    assert_eq!(first.worker_id(), reused.worker_id());
    assert_eq!(fixture.counts().create_calls, 1);
    assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 0);
    let issue = handle
        .ensure_native_interactive_worker(configured)
        .await
        .unwrap();
    assert_ne!(first.worker_id(), issue.worker_id());
    assert_eq!(fixture.counts().create_calls, 2);
    let shell = handle
        .start_native_profile_settings_terminal(&first)
        .await
        .unwrap();
    assert!(shell.job_id().value > 0);
    assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 1);
    let request = fixture.job_requests().last().unwrap().clone();
    assert_eq!(
        request.worker,
        Some(
            jobworkerp_client::jobworkerp::service::job_request::Worker::WorkerId(
                *first.worker_id()
            )
        )
    );
    assert_eq!(request.using.as_deref(), Some("run_with_client"));
    let args = JobworkerpProto::parse_job_args_schema_descriptor(
        &fixture.runner_data(),
        Some("run_with_client"),
    )
    .unwrap()
    .unwrap();
    let args = prost_reflect::DynamicMessage::decode(args, request.args.as_slice())
        .unwrap()
        .serialize(serde_json::value::Serializer)
        .unwrap();
    assert_eq!(args["command"], "/bin/sh");
    assert_eq!(
        args["args"],
        json!([
            "-c",
            lookback_tauri_lib::jobworkerp::coding_agent::PROFILE_SETTINGS_SHELL_SCRIPT
        ])
    );
    assert_eq!(args["workingDir"], "/workspace");
}

#[tokio::test]
async fn a_changed_profile_settings_worker_never_starts_the_authentication_shell() {
    use lookback_tauri_lib::jobworkerp::coding_agent::NativeProfileSettingsWorkerSpec;
    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let configured = spec();
    let settings = NativeProfileSettingsWorkerSpec {
        native_profile_id: configured.native_profile_id,
        use_static: configured.use_static,
        workspace_binding: configured.workspace_binding,
        runner_settings_json: configured.runner_settings_json,
        host_context: configured.host_context,
    };
    let matched = handle
        .ensure_native_profile_settings_worker(settings)
        .await
        .unwrap();
    fixture.mutate_first_worker(|worker| worker.use_static = !worker.use_static);
    assert!(
        handle
            .start_native_profile_settings_terminal(&matched)
            .await
            .is_err()
    );
    assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 0);
}
