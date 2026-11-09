use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use coding_agent_executor::{
    BaseResolutionReceipt, PROTOCOL_VERSION, PreparedWorkspace, RepositoryId,
};
use jobworkerp_client::{
    jobworkerp::data::{ResultOutputItem, result_output_item::Item},
    proto::JobworkerpProto,
};
use lookback_tauri_lib::{
    coding_agent::{
        AgentProfile, AgentProfileSnapshot, CloneTransport, ForgeKind, HostResourceLimits,
        RepositoryHostConfig, RepositoryResolution, ResolvedRepository, SandboxResources,
        resolve_repository,
    },
    grpc::proto::jobworkerp::runner::sandbox::{
        SandboxExecExit, SandboxExecOutput, SandboxExecResult,
        sandbox_exec_result::Result as SandboxResult,
    },
    jobworkerp::{
        JobworkerpHandle,
        coding_agent::{HostWorkerContext, NativeInteractiveWorkerSpec, NativeWorkspaceBinding},
    },
    native_terminal::{
        CanonicalIssueSnapshot, NativeAuthoritySnapshot, NativeSessionAuthority,
        NativeSessionContext, NativeSessionContextInput, NativeTerminalManager,
        NativeTerminalManagerError, NativeTerminalOutputEvent, NativeTerminalOutputStore,
        NativeTerminalPreview, NativeTerminalStore, NativeWorkerBinding, SessionState,
        launch::{NativeAgentKind, NativeCliProfile, WorkspaceMountBinding, build_launch_plan},
    },
};
use prost::Message;
use serde::Serialize as _;
use serde_json::json;
use tokio::sync::{Mutex, Notify};

#[path = "fixtures/native_interactive_rpc.rs"]
mod native_interactive_rpc;

use native_interactive_rpc::{RpcFixture, TestServer, start_server};

const IMAGE_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CLI_VERSION: &str = "1.18.3";
const OWNER_WINDOW: &str = "main";

fn private_dir(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn uuid(value: u8) -> String {
    format!("00000000-0000-4000-8000-{value:012x}")
}

fn repository() -> ResolvedRepository {
    let config = RepositoryHostConfig {
        label_host: "forge.example".into(),
        forge: ForgeKind::Gitea,
        api_base_url: "https://forge.example/api/v1".into(),
        clone_transport: CloneTransport::Https {
            base_url: "https://forge.example/git".into(),
        },
        read_credential_ref: None,
        publication_credential_ref: None,
    };
    match resolve_repository(&["repo:forge.example/team/repo".into()], &[config], None) {
        RepositoryResolution::Ready(repository) => repository,
        other => panic!("unexpected repository resolution: {other:?}"),
    }
}

fn profile(root: &Path, repository: &ResolvedRepository, use_static: bool) -> AgentProfileSnapshot {
    let image_ref = format!("registry.example/agents/opencode@{IMAGE_DIGEST}");
    AgentProfile {
        profile_id: "opencode-native".into(),
        version: 3,
        display_name: "OpenCode".into(),
        image_ref: image_ref.clone(),
        allowed_image_refs: vec![image_ref],
        command: vec!["opencode".into(), "run".into()],
        container_home: "/home/codespace".into(),
        managed_home_overlay: None,
        runtime_environment: Default::default(),
        network_access: Default::default(),
        resources: SandboxResources {
            cpu_millis: 1000,
            memory_mib: 2048,
            root_disk_mib: 8192,
            max_parallel_runs: 1,
            use_static,
        },
        mount_allowed_roots: vec![root.to_path_buf()],
        persistent_mounts: Vec::new(),
        allowed_repositories: vec![repository.identity.clone()],
        credential_reference: None,
    }
    .validate_and_snapshot(&HostResourceLimits {
        cpu_millis: 4000,
        memory_mib: 8192,
        root_disk_mib: 32768,
        max_parallel_runs: 2,
    })
    .unwrap()
}

fn worker_spec(workspace_root: &Path) -> NativeInteractiveWorkerSpec {
    worker_spec_with_static(workspace_root, true)
}

fn worker_spec_with_static(workspace_root: &Path, use_static: bool) -> NativeInteractiveWorkerSpec {
    let root = workspace_root.to_string_lossy().into_owned();
    let image_ref = format!("registry.example/agents/opencode@{IMAGE_DIGEST}");
    NativeInteractiveWorkerSpec {
        native_profile_id: "opencode-native".into(),
        repository_id: RepositoryId {
            host: "forge.example".into(),
            owner: "team".into(),
            name: "repo".into(),
        },
        use_static,
        workspace_binding: NativeWorkspaceBinding {
            canonical_host_root: workspace_root.to_path_buf(),
            guest_root: "/workspace".into(),
        },
        runner_settings_json: json!({
            "vm": {
                "image": image_ref,
                "cpus": 1,
                "memory_mib": 2048,
                "root_disk_mib": 8192,
                "mounts": [{
                    "host_path": root,
                    "guest_path": "/workspace",
                    "writable": true,
                    "executable": true
                }],
                "env": {"HOME": "/home/codespace"}
            },
            "allowed_images": [image_ref],
            "allowed_host_mounts": [{
                "host_root": root,
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

fn make_context(
    root: &Path,
    matched: &lookback_tauri_lib::jobworkerp::coding_agent::NativeWorkerMatch,
    worker_name: &str,
    use_static: bool,
) -> NativeSessionContext {
    let repository = repository();
    let workspace_root = root.join("workspace-mount");
    std::fs::create_dir_all(&workspace_root).unwrap();
    let workspace_path = workspace_root.join("issue-42");
    std::fs::create_dir_all(&workspace_path).unwrap();
    let repository_id = RepositoryId {
        host: repository.identity.host.clone(),
        owner: repository.identity.organization.clone(),
        name: repository.identity.repository.clone(),
    };
    let session_id = uuid(1);
    let attempt_id = uuid(2);
    let issue = CanonicalIssueSnapshot::new(
        "issue-42",
        11,
        "Run the native interactive agent",
        "Keep the exact issue body.\nSecond line.",
        vec![
            "Preserve the exit status".into(),
            "Keep raw terminal output".into(),
        ],
    )
    .unwrap();
    let profile = profile(&workspace_root, &repository, use_static);
    let base_commit = "b".repeat(40);
    let base_resolution_receipt = BaseResolutionReceipt {
        run_id: session_id.clone(),
        attempt_id: attempt_id.clone(),
        operation_id: uuid(90),
        repository: repository_id.clone(),
        base_branch: "main".into(),
        base_commit: base_commit.clone(),
        trusted_config_digest: "c".repeat(64),
        protocol_version: PROTOCOL_VERSION,
        executor_version: "0.2.0".into(),
    };
    let prepared_workspace = PreparedWorkspace {
        run_id: session_id.clone(),
        attempt_id: attempt_id.clone(),
        repository: repository_id,
        base_branch: "main".into(),
        base_commit,
        branch: "native/issue-42".into(),
        workspace_path: workspace_path.to_string_lossy().into_owned(),
    };
    let cli_profile = NativeCliProfile {
        kind: NativeAgentKind::OpenCode,
        cli_version: CLI_VERSION.into(),
        runtime_image_digest: profile.image_digest.clone(),
        expected_profile_command: profile.command.clone(),
    };
    let workspace_mount_binding = WorkspaceMountBinding {
        canonical_host_root: workspace_root,
        guest_root: "/workspace".into(),
    };
    let worker_binding = NativeWorkerBinding {
        worker_id: matched.worker_id().value,
        runner_id: matched.runner_id().value,
        worker_name: worker_name.into(),
        settings_digest: matched.settings_digest().into(),
        image_digest: profile.image_digest.clone(),
    };
    let launch_plan = build_launch_plan(
        &profile,
        &cli_profile,
        &prepared_workspace,
        &workspace_mount_binding,
        &issue,
        None,
    )
    .unwrap();
    let preview = NativeTerminalPreview::from_launch(
        uuid(3),
        14,
        issue.revision(),
        Some(repository.identity.clone()),
        &launch_plan,
        Some(worker_binding),
    )
    .unwrap();

    NativeSessionContext::new(NativeSessionContextInput {
        session_id,
        attempt_id,
        owner_window_label: OWNER_WINDOW.into(),
        canonical_issue: issue,
        repository,
        base_resolution_receipt,
        prepared_workspace,
        profile,
        cli_profile,
        workspace_mount_binding,
        launch_plan,
        preview,
    })
    .unwrap()
}

#[derive(Clone)]
struct TestAuthority(Arc<StdMutex<NativeAuthoritySnapshot>>);

#[async_trait::async_trait]
impl NativeSessionAuthority for TestAuthority {
    async fn read_current(
        &self,
        _expected: &NativeSessionContext,
    ) -> Result<NativeAuthoritySnapshot, lookback_tauri_lib::native_terminal::NativeAuthorityError>
    {
        Ok(self.0.lock().unwrap().clone())
    }
}

struct UnavailableAuthority;

#[async_trait::async_trait]
impl NativeSessionAuthority for UnavailableAuthority {
    async fn read_current(
        &self,
        _expected: &NativeSessionContext,
    ) -> Result<NativeAuthoritySnapshot, lookback_tauri_lib::native_terminal::NativeAuthorityError>
    {
        Err(lookback_tauri_lib::native_terminal::NativeAuthorityError::Unavailable)
    }
}

struct Harness {
    _server: TestServer,
    fixture: RpcFixture,
    handle: JobworkerpHandle,
    matched: lookback_tauri_lib::jobworkerp::coding_agent::NativeWorkerMatch,
    manager: NativeTerminalManager,
    context: NativeSessionContext,
    authority: TestAuthority,
    root: tempfile::TempDir,
}

async fn harness() -> Harness {
    harness_with_static(true).await
}

async fn harness_with_static(use_static: bool) -> Harness {
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let workspace_root = root.path().join("workspace-mount");
    std::fs::create_dir_all(&workspace_root).unwrap();
    let app_private_root = root.path().join("app-private");
    private_dir(&app_private_root);

    let fixture = RpcFixture::default();
    let server = start_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let matched = handle
        .ensure_native_interactive_worker(worker_spec_with_static(&workspace_root, use_static))
        .await
        .unwrap();
    let worker_name = fixture.created_workers()[0].name.clone();
    let context = make_context(root.path(), &matched, &worker_name, use_static);
    let authority = TestAuthority(Arc::new(StdMutex::new(NativeAuthoritySnapshot::from_host(
        context.canonical_issue().clone(),
        context.repository().clone(),
        context.profile().clone(),
        context.preview().expected_settings_revision(),
    ))));

    let store = Arc::new(
        NativeTerminalStore::open(&app_private_root.join("native-terminal.sqlite3")).unwrap(),
    );
    let output = Arc::new(
        NativeTerminalOutputStore::open(&app_private_root.join("native-terminal-output.sqlite3"))
            .unwrap(),
    );
    let manager = NativeTerminalManager::new(
        store,
        output,
        handle.clone(),
        Arc::new(authority.clone()),
        Arc::new(Mutex::new(())),
    );
    manager.register_preview(context.clone(), &matched).unwrap();

    Harness {
        _server: server,
        fixture,
        handle,
        matched,
        manager,
        context,
        authority,
        root,
    }
}

#[derive(Clone)]
struct BlockingAuthority {
    snapshot: NativeAuthoritySnapshot,
    entered: Arc<Notify>,
    resume: Arc<Notify>,
}

#[async_trait::async_trait]
impl NativeSessionAuthority for BlockingAuthority {
    async fn read_current(
        &self,
        _expected: &NativeSessionContext,
    ) -> Result<NativeAuthoritySnapshot, lookback_tauri_lib::native_terminal::NativeAuthorityError>
    {
        self.entered.notify_one();
        self.resume.notified().await;
        Ok(self.snapshot.clone())
    }
}

fn result_item(result: SandboxResult) -> ResultOutputItem {
    ResultOutputItem {
        item: Some(Item::Data(
            SandboxExecResult {
                result: Some(result),
            }
            .encode_to_vec(),
        )),
    }
}

fn clean_result_stream(fixture: &RpcFixture, exit_code: i32) {
    fixture.set_job_result_items(vec![
        result_item(SandboxResult::Output(SandboxExecOutput {
            stream: "stdout".into(),
            data: vec![0, 0xff, b'\n', 0, b'<', b'x', b'>'],
        })),
        result_item(SandboxResult::Exit(SandboxExecExit {
            exit_code,
            execution_time_ms: 321,
            sandbox_id: "native-sandbox".into(),
        })),
        ResultOutputItem {
            item: Some(Item::End(Default::default())),
        },
    ]);
}

async fn wait_for_end(
    harness: &Harness,
) -> Vec<lookback_tauri_lib::native_terminal::NativeTerminalOutputRecord> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let page = harness
                .manager
                .output_page(OWNER_WINDOW, harness.context.session_id(), 0, 16)
                .unwrap();
            if page
                .iter()
                .any(|record| record.event == NativeTerminalOutputEvent::End)
            {
                return page;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the manager drains the native result stream through End and EOF")
}

#[tokio::test]
async fn accepted_session_uses_the_verified_worker_once_and_replays_exact_native_output() {
    let harness = harness().await;
    clean_result_stream(&harness.fixture, 7);

    let first = harness
        .manager
        .start(
            OWNER_WINDOW,
            harness.context.preview().preview_id(),
            harness.context.preview().preview_digest(),
        )
        .await
        .unwrap();
    assert_eq!(first.native_job_id, Some(9_004));
    assert!(matches!(
        first.state,
        SessionState::Running | SessionState::Closed
    ));

    let request = harness.fixture.job_requests().pop().unwrap();
    assert!(request.uniq_key.is_none());
    assert_eq!(request.using.as_deref(), Some("run_with_client"));
    let descriptor = JobworkerpProto::parse_job_args_schema_descriptor(
        &harness.fixture.runner_data(),
        Some("run_with_client"),
    )
    .unwrap()
    .unwrap();
    let decoded = prost_reflect::DynamicMessage::decode(descriptor, request.args.as_slice())
        .unwrap()
        .serialize(serde_json::value::Serializer)
        .unwrap();
    assert_eq!(decoded["command"], "opencode");
    assert_eq!(decoded["workingDir"], "/workspace/issue-42");
    assert_eq!(decoded["args"].as_array().unwrap().len(), 1);
    assert_eq!(
        decoded["args"][0],
        format!("--prompt={}", harness.context.initial_prompt().text())
    );

    let replay = wait_for_end(&harness).await;
    assert_eq!(replay.len(), 3);
    assert_eq!(
        replay[0].event,
        NativeTerminalOutputEvent::Output {
            stream: "stdout".into(),
            data: vec![0, 0xff, b'\n', 0, b'<', b'x', b'>'],
        }
    );
    assert_eq!(
        replay[1].event,
        NativeTerminalOutputEvent::Exit {
            exit_code: 7,
            execution_time_ms: 321,
            sandbox_id: "native-sandbox".into(),
        }
    );
    assert_eq!(replay[2].event, NativeTerminalOutputEvent::End);
    let terminal = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let current = harness
                .manager
                .session(OWNER_WINDOW, harness.context.session_id())
                .unwrap()
                .unwrap();
            if matches!(current.state, SessionState::Closed | SessionState::Unknown) {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("terminal lifecycle facts are persisted after the output End");
    assert_eq!(terminal.state, SessionState::Closed);
    assert_eq!(terminal.exit_code, Some(7));
    assert!(terminal.end_received);
    assert!(terminal.stream_eof);
    assert!(!terminal.stream_error);

    let duplicate = harness
        .manager
        .start(
            OWNER_WINDOW,
            harness.context.preview().preview_id(),
            harness.context.preview().preview_digest(),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.session_id, first.session_id);
    assert_eq!(harness.fixture.counts().job_enqueue_calls, 1);
    assert_eq!(
        harness
            .manager
            .output_page(OWNER_WINDOW, harness.context.session_id(), 0, 16)
            .unwrap(),
        replay
    );
}

#[tokio::test]
async fn foreign_owner_and_stale_authority_are_rejected_before_any_job_rpc() {
    let harness = harness().await;
    assert!(matches!(
        harness
            .manager
            .start(
                "secondary-window",
                harness.context.preview().preview_id(),
                harness.context.preview().preview_digest(),
            )
            .await,
        Err(NativeTerminalManagerError::OwnerMismatch)
    ));

    {
        let mut current = harness.authority.0.lock().unwrap();
        current.settings_revision += 1;
    }
    assert!(matches!(
        harness
            .manager
            .start(
                OWNER_WINDOW,
                harness.context.preview().preview_id(),
                harness.context.preview().preview_digest(),
            )
            .await,
        Err(NativeTerminalManagerError::AuthorityChanged)
    ));

    {
        let mut current = harness.authority.0.lock().unwrap();
        current.settings_revision = harness.context.preview().expected_settings_revision();
        current.canonical_issue = CanonicalIssueSnapshot::new(
            "issue-42",
            11,
            "Same revision, changed host-owned title",
            "Keep the exact issue body.\nSecond line.",
            vec![
                "Preserve the exit status".into(),
                "Keep raw terminal output".into(),
            ],
        )
        .unwrap();
    }
    assert!(matches!(
        harness
            .manager
            .start(
                OWNER_WINDOW,
                harness.context.preview().preview_id(),
                harness.context.preview().preview_digest(),
            )
            .await,
        Err(NativeTerminalManagerError::AuthorityChanged)
    ));

    {
        let mut current = harness.authority.0.lock().unwrap();
        current.canonical_issue = harness.context.canonical_issue().clone();
        current.profile.version += 1;
    }
    assert!(matches!(
        harness
            .manager
            .start(
                OWNER_WINDOW,
                harness.context.preview().preview_id(),
                harness.context.preview().preview_digest(),
            )
            .await,
        Err(NativeTerminalManagerError::AuthorityChanged)
    ));
    assert_eq!(harness.fixture.counts().job_enqueue_calls, 0);
}

#[tokio::test]
async fn worker_match_scope_must_bind_the_prepared_profile_repository_workspace_image_and_resources()
 {
    let harness = harness().await;
    let base_root = harness.root.path().join("workspace-mount");
    let mut mismatched_specs = Vec::new();

    let mut other_profile = worker_spec(&base_root);
    other_profile.native_profile_id = "different-profile".into();
    mismatched_specs.push(other_profile);

    let mut other_repository = worker_spec(&base_root);
    other_repository.repository_id.name = "different-repository".into();
    mismatched_specs.push(other_repository);

    let other_root = harness.root.path().join("different-workspace-mount");
    std::fs::create_dir_all(&other_root).unwrap();
    let other_root_string = other_root.to_string_lossy().into_owned();
    let mut other_workspace = worker_spec(&base_root);
    other_workspace.workspace_binding.canonical_host_root = other_root.clone();
    other_workspace.runner_settings_json["vm"]["mounts"][0]["host_path"] = json!(other_root_string);
    other_workspace.runner_settings_json["allowed_host_mounts"][0]["host_root"] =
        json!(other_root_string);
    mismatched_specs.push(other_workspace);

    let mut other_image = worker_spec(&base_root);
    let replacement_image = format!("registry.example/agents/opencode@sha256:{}", "b".repeat(64));
    other_image.runner_settings_json["vm"]["image"] = json!(replacement_image);
    other_image.runner_settings_json["allowed_images"] = json!([replacement_image]);
    mismatched_specs.push(other_image);

    let mut other_resources = worker_spec(&base_root);
    other_resources.runner_settings_json["vm"]["memory_mib"] = json!(3072);
    mismatched_specs.push(other_resources);

    for spec in mismatched_specs {
        let matched = harness
            .handle
            .ensure_native_interactive_worker(spec)
            .await
            .unwrap();
        let worker_name = harness
            .fixture
            .created_workers()
            .last()
            .unwrap()
            .name
            .clone();
        let context = make_context(harness.root.path(), &matched, &worker_name, true);

        assert!(matches!(
            harness.manager.register_preview(context, &matched),
            Err(NativeTerminalManagerError::WorkerMismatch)
        ));
    }

    assert_eq!(harness.fixture.counts().job_enqueue_calls, 0);
}

#[tokio::test]
async fn a_dynamic_profile_keeps_non_static_worker_policy_and_reuses_its_worker() {
    let harness = harness_with_static(false).await;
    assert!(!harness.context.profile().resources.use_static);
    assert!(!harness.matched.scope().use_static());
    assert!(!harness.fixture.created_workers()[0].use_static);

    let workspace_root = harness.root.path().join("workspace-mount");
    let second_match = harness
        .handle
        .ensure_native_interactive_worker(worker_spec_with_static(&workspace_root, false))
        .await
        .unwrap();
    assert_eq!(harness.matched.worker_id(), second_match.worker_id());
    assert_eq!(harness.fixture.counts().create_calls, 1);

    clean_result_stream(&harness.fixture, 0);
    harness
        .manager
        .start(
            OWNER_WINDOW,
            harness.context.preview().preview_id(),
            harness.context.preview().preview_digest(),
        )
        .await
        .unwrap();
    assert_eq!(harness.fixture.counts().job_enqueue_calls, 1);
    assert_eq!(harness.fixture.counts().create_calls, 1);
    assert_eq!(harness.fixture.created_workers().len(), 1);
    wait_for_end(&harness).await;
}

#[tokio::test]
async fn a_non_static_profile_rejects_a_match_bound_to_static_worker_settings() {
    let harness = harness_with_static(false).await;
    let workspace_root = harness.root.path().join("workspace-mount");
    let static_match = harness
        .handle
        .ensure_native_interactive_worker(worker_spec_with_static(&workspace_root, true))
        .await
        .unwrap();
    let static_worker_name = harness
        .fixture
        .created_workers()
        .last()
        .unwrap()
        .name
        .clone();
    let non_static_context = make_context(
        harness.root.path(),
        &static_match,
        &static_worker_name,
        false,
    );

    assert!(matches!(
        harness
            .manager
            .register_preview(non_static_context, &static_match),
        Err(NativeTerminalManagerError::WorkerMismatch)
    ));
    assert_eq!(harness.fixture.counts().job_enqueue_calls, 0);
}

#[tokio::test]
async fn unavailable_canonical_authority_does_not_claim_or_enqueue_a_session() {
    let harness = harness().await;
    let app_private_root = harness.root.path().join("app-private");
    let manager = NativeTerminalManager::new(
        Arc::new(
            NativeTerminalStore::open(&app_private_root.join("native-terminal.sqlite3")).unwrap(),
        ),
        Arc::new(
            NativeTerminalOutputStore::open(
                &app_private_root.join("native-terminal-output.sqlite3"),
            )
            .unwrap(),
        ),
        harness.handle.clone(),
        Arc::new(UnavailableAuthority),
        harness.manager.admission_lock(),
    );
    manager
        .register_preview(harness.context.clone(), &harness.matched)
        .unwrap();

    assert!(matches!(
        manager
            .start(
                OWNER_WINDOW,
                harness.context.preview().preview_id(),
                harness.context.preview().preview_digest(),
            )
            .await,
        Err(NativeTerminalManagerError::AuthorityUnavailable)
    ));
    assert_eq!(harness.fixture.counts().job_enqueue_calls, 0);
    assert!(
        manager
            .session(OWNER_WINDOW, harness.context.session_id())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn settings_admission_writer_waits_until_the_native_start_claim_is_resolved() {
    let harness = harness().await;
    let app_private_root = harness.root.path().join("app-private");
    let admission = Arc::new(Mutex::new(()));
    let authority = Arc::new(BlockingAuthority {
        snapshot: NativeAuthoritySnapshot::from_host(
            harness.context.canonical_issue().clone(),
            harness.context.repository().clone(),
            harness.context.profile().clone(),
            harness.context.preview().expected_settings_revision(),
        ),
        entered: Arc::new(Notify::new()),
        resume: Arc::new(Notify::new()),
    });
    let manager = NativeTerminalManager::new(
        Arc::new(
            NativeTerminalStore::open(&app_private_root.join("native-terminal.sqlite3")).unwrap(),
        ),
        Arc::new(
            NativeTerminalOutputStore::open(
                &app_private_root.join("native-terminal-output.sqlite3"),
            )
            .unwrap(),
        ),
        harness.handle.clone(),
        authority.clone(),
        admission.clone(),
    );
    manager
        .register_preview(harness.context.clone(), &harness.matched)
        .unwrap();

    let start_manager = manager.clone();
    let preview_id = harness.context.preview().preview_id().to_owned();
    let preview_digest = harness.context.preview().preview_digest().to_owned();
    let started = tokio::spawn(async move {
        start_manager
            .start(OWNER_WINDOW, &preview_id, &preview_digest)
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), authority.entered.notified())
        .await
        .expect("native start revalidates host state while holding admission");

    let write_admitted = Arc::new(AtomicBool::new(false));
    let write_flag = Arc::clone(&write_admitted);
    let write_lock = Arc::clone(&admission);
    let writer = tokio::spawn(async move {
        let _guard = write_lock.lock().await;
        write_flag.store(true, Ordering::SeqCst);
    });
    tokio::task::yield_now().await;
    assert!(!write_admitted.load(Ordering::SeqCst));

    authority.resume.notify_one();
    started.await.unwrap().unwrap();
    writer.await.unwrap();
    assert!(write_admitted.load(Ordering::SeqCst));
    assert_eq!(harness.fixture.counts().job_enqueue_calls, 1);
}

#[tokio::test]
async fn ambiguous_enqueue_is_unknown_and_duplicate_start_never_resends_it() {
    let harness = harness().await;
    harness.fixture.set_enqueue_unavailable();
    let preview_id = harness.context.preview().preview_id();
    let preview_digest = harness.context.preview().preview_digest();

    assert!(
        harness
            .manager
            .start(OWNER_WINDOW, preview_id, preview_digest)
            .await
            .is_err()
    );
    let record = harness
        .manager
        .session(OWNER_WINDOW, harness.context.session_id())
        .unwrap()
        .unwrap();
    assert_eq!(record.state, SessionState::Unknown);

    let replay = harness
        .manager
        .start(OWNER_WINDOW, preview_id, preview_digest)
        .await
        .unwrap();
    assert_eq!(replay.state, SessionState::Unknown);
    assert_eq!(harness.fixture.counts().job_enqueue_calls, 1);

    let app_private_root = harness.root.path().join("app-private");
    let restarted = NativeTerminalManager::new(
        Arc::new(
            NativeTerminalStore::open(&app_private_root.join("native-terminal.sqlite3")).unwrap(),
        ),
        Arc::new(
            NativeTerminalOutputStore::open(
                &app_private_root.join("native-terminal-output.sqlite3"),
            )
            .unwrap(),
        ),
        harness.handle.clone(),
        Arc::new(harness.authority.clone()),
        harness.manager.admission_lock(),
    );
    assert_eq!(
        restarted
            .recover_for_window(OWNER_WINDOW)
            .unwrap()
            .first()
            .unwrap()
            .state,
        SessionState::Unknown
    );
    assert_eq!(
        restarted
            .start(OWNER_WINDOW, preview_id, preview_digest)
            .await
            .unwrap()
            .state,
        SessionState::Unknown
    );
    assert_eq!(harness.fixture.counts().job_enqueue_calls, 1);
}

#[tokio::test]
async fn stream_error_stays_unknown_and_cancel_dispatch_is_claimed_once() {
    let harness = harness().await;
    harness.fixture.set_job_result_items(Vec::new());
    let record = harness
        .manager
        .start(
            OWNER_WINDOW,
            harness.context.preview().preview_id(),
            harness.context.preview().preview_digest(),
        )
        .await
        .unwrap();
    assert_eq!(record.native_job_id, Some(9_004));

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let current = harness
                .manager
                .session(OWNER_WINDOW, harness.context.session_id())
                .unwrap()
                .unwrap();
            if current.state == SessionState::Unknown {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("transport EOF without End is unresolved, not a clean close");

    let app_private_root = harness.root.path().join("app-private");
    let second_manager = NativeTerminalManager::new(
        Arc::new(
            NativeTerminalStore::open(&app_private_root.join("native-terminal.sqlite3")).unwrap(),
        ),
        Arc::new(
            NativeTerminalOutputStore::open(
                &app_private_root.join("native-terminal-output.sqlite3"),
            )
            .unwrap(),
        ),
        harness.handle.clone(),
        Arc::new(harness.authority.clone()),
        harness.manager.admission_lock(),
    );
    let (first_cancel, second_cancel) = tokio::join!(
        harness
            .manager
            .cancel(OWNER_WINDOW, harness.context.session_id()),
        second_manager.cancel(OWNER_WINDOW, harness.context.session_id())
    );
    assert_ne!(first_cancel.is_err(), second_cancel.is_err());
    assert_eq!(harness.fixture.counts().job_delete_calls, 1);
}

#[tokio::test]
async fn cancellation_during_enqueue_waits_for_the_durable_job_id_and_dispatches_once() {
    let harness = harness().await;
    let gate = harness.fixture.block_next_enqueue();
    let manager = harness.manager.clone();
    let preview_id = harness.context.preview().preview_id().to_owned();
    let preview_digest = harness.context.preview().preview_digest().to_owned();
    let start = tokio::spawn(async move {
        manager
            .start(OWNER_WINDOW, &preview_id, &preview_digest)
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), gate.wait_until_entered())
        .await
        .expect("the enqueue request reaches the delayed acceptance boundary");

    let cancellation = harness
        .manager
        .cancel(OWNER_WINDOW, harness.context.session_id())
        .await
        .unwrap();
    assert!(cancellation.cancel_requested);
    assert_eq!(cancellation.native_job_id, None);
    assert_eq!(harness.fixture.counts().job_delete_calls, 0);

    gate.resume();
    let acceptance_dispatch = tokio::time::timeout(Duration::from_secs(2), start)
        .await
        .expect("the delayed native acceptance completes")
        .unwrap();
    assert!(matches!(
        acceptance_dispatch,
        Err(NativeTerminalManagerError::Transport)
    ));
    let accepted = harness
        .manager
        .session(OWNER_WINDOW, harness.context.session_id())
        .unwrap()
        .unwrap();
    assert_eq!(accepted.native_job_id, Some(9_004));
    assert_eq!(harness.fixture.counts().job_delete_calls, 1);
}

#[tokio::test]
async fn shutdown_is_scoped_to_the_real_manager_and_pending_start_keeps_cancel_intent() {
    let harness = harness().await;
    let enqueue_gate = harness.fixture.block_next_enqueue();
    clean_result_stream(&harness.fixture, 7);
    let manager = harness.manager.clone();
    let preview_id = harness.context.preview().preview_id().to_owned();
    let preview_digest = harness.context.preview().preview_digest().to_owned();
    let start = tokio::spawn(async move {
        manager
            .start(OWNER_WINDOW, &preview_id, &preview_digest)
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), enqueue_gate.wait_until_entered())
        .await
        .expect("the start RPC reaches its pending acceptance boundary");

    let app_private_root = harness.root.path().join("app-private");
    let cold_manager = NativeTerminalManager::new(
        Arc::new(
            NativeTerminalStore::open(&app_private_root.join("native-terminal.sqlite3")).unwrap(),
        ),
        Arc::new(
            NativeTerminalOutputStore::open(
                &app_private_root.join("native-terminal-output.sqlite3"),
            )
            .unwrap(),
        ),
        harness.handle.clone(),
        Arc::new(harness.authority.clone()),
        harness.manager.admission_lock(),
    );
    let cold_shutdown = cold_manager
        .shutdown_for_window(OWNER_WINDOW, Duration::from_millis(30))
        .await
        .unwrap();
    assert_eq!(cold_shutdown[0].state, SessionState::Starting);
    assert!(!cold_shutdown[0].cancel_requested);
    assert_eq!(harness.fixture.counts().job_delete_calls, 0);

    let owned_shutdown = harness
        .manager
        .shutdown_for_window(OWNER_WINDOW, Duration::from_millis(30))
        .await
        .unwrap();
    assert_eq!(owned_shutdown[0].state, SessionState::Unknown);
    assert!(owned_shutdown[0].cancel_requested);
    assert_eq!(owned_shutdown[0].native_job_id, None);
    assert_eq!(harness.fixture.counts().job_delete_calls, 0);

    enqueue_gate.resume();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), start)
            .await
            .expect("late acceptance resolves the start owner")
            .unwrap(),
        Err(NativeTerminalManagerError::Transport)
    ));
    assert_eq!(harness.fixture.counts().job_delete_calls, 1);

    let terminal = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let record = harness
                .manager
                .session(OWNER_WINDOW, harness.context.session_id())
                .unwrap()
                .unwrap();
            if record.state == SessionState::Cancelled {
                break record;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("only real Exit, End, and EOF facts resolve the canceled session");
    assert_eq!(terminal.exit_code, Some(7));
    assert!(terminal.end_received);
    assert!(terminal.stream_eof);
    assert_eq!(harness.fixture.counts().job_delete_calls, 1);
}

#[tokio::test]
async fn failed_job_id_persistence_stays_unknown_without_ram_cancel_fallback() {
    let harness = harness().await;
    let gate = harness.fixture.block_next_enqueue();
    let manager = harness.manager.clone();
    let preview_id = harness.context.preview().preview_id().to_owned();
    let preview_digest = harness.context.preview().preview_digest().to_owned();
    let start = tokio::spawn(async move {
        manager
            .start(OWNER_WINDOW, &preview_id, &preview_digest)
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), gate.wait_until_entered())
        .await
        .expect("the enqueue request reaches the delayed acceptance boundary");
    harness
        .manager
        .cancel(OWNER_WINDOW, harness.context.session_id())
        .await
        .unwrap();

    let database_path = harness
        .root
        .path()
        .join("app-private/native-terminal.sqlite3");
    let connection = rusqlite::Connection::open(database_path).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_native_job_accept
             BEFORE UPDATE OF native_job_id ON native_terminal_sessions
             BEGIN SELECT RAISE(ABORT, 'injected acceptance persistence failure'); END;",
        )
        .unwrap();

    gate.resume();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), start)
            .await
            .expect("the delayed native acceptance completes")
            .unwrap(),
        Err(NativeTerminalManagerError::Store(_))
    ));
    let reconciled = harness
        .manager
        .cancel(OWNER_WINDOW, harness.context.session_id())
        .await
        .unwrap();
    assert_eq!(reconciled.state, SessionState::Unknown);
    assert_eq!(reconciled.native_job_id, None);
    assert!(reconciled.cancel_requested);
    assert_eq!(harness.fixture.counts().job_delete_calls, 0);
}

#[tokio::test]
async fn live_display_attachment_does_not_change_stream_ownership() {
    let harness = harness().await;
    clean_result_stream(&harness.fixture, 0);
    harness
        .manager
        .start(
            OWNER_WINDOW,
            harness.context.preview().preview_id(),
            harness.context.preview().preview_digest(),
        )
        .await
        .unwrap();
    let expected = wait_for_end(&harness).await;

    let detached = harness
        .manager
        .attach(OWNER_WINDOW, harness.context.session_id(), 0, 16)
        .unwrap();
    assert_eq!(detached.events, expected);
    drop(detached);

    let reattached = harness
        .manager
        .attach(OWNER_WINDOW, harness.context.session_id(), 0, 16)
        .unwrap();
    assert_eq!(reattached.events, expected);
    assert!(matches!(
        harness
            .manager
            .attach("secondary-window", harness.context.session_id(), 0, 16),
        Err(NativeTerminalManagerError::OwnerMismatch)
    ));
    assert_eq!(harness.fixture.counts().job_delete_calls, 0);
}
