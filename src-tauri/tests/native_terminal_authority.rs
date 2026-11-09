use std::{path::Path, sync::Arc};

use coding_agent_executor::{
    BaseResolutionReceipt, PROTOCOL_VERSION, PreparedWorkspace, RepositoryId,
};
use lookback_tauri_lib::{
    coding_agent::{
        AgentProfile, AgentProfileSnapshot, ForgeKind, HostResourceLimits, RepositoryHostConfig,
        RepositoryResolution, ResolvedRepository, SandboxResources, resolve_repository,
    },
    commands::IssueTrackerAvailability,
    data::DataPaths,
    native_terminal::{
        CanonicalIssueSnapshot, ManagedNativeSessionAuthority, NativeAuthorityError,
        NativeSessionAuthority, NativeSessionContext, NativeSessionContextInput,
        NativeTerminalPreview, NativeWorkerBinding,
        launch::{NativeAgentKind, NativeCliProfile, WorkspaceMountBinding, build_launch_plan},
    },
    sidecar::{SidecarConfig, Sidecars},
};

const IMAGE_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn uuid(value: u8) -> String {
    format!("00000000-0000-4000-8000-{value:012x}")
}

fn repository() -> ResolvedRepository {
    let config = RepositoryHostConfig {
        label_host: "forge.example".into(),
        forge: ForgeKind::Gitea,
        api_base_url: "https://forge.example/api/v1".into(),
        clone_transport: lookback_tauri_lib::coding_agent::CloneTransport::Https {
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

fn profile(root: &Path, repository: &ResolvedRepository) -> AgentProfileSnapshot {
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
            use_static: false,
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

fn context(root: &Path) -> NativeSessionContext {
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
    let profile = profile(&workspace_root, &repository);
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
        cli_version: "1.18.3".into(),
        runtime_image_digest: profile.image_digest.clone(),
        expected_profile_command: profile.command.clone(),
    };
    let workspace_mount_binding = WorkspaceMountBinding {
        canonical_host_root: workspace_root,
        guest_root: "/workspace".into(),
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
    let worker_binding = NativeWorkerBinding {
        worker_id: 1,
        runner_id: 1,
        worker_name: "lookback.interactive.0123456789abcdef0123456789abcdef".into(),
        settings_digest: "d".repeat(64),
        image_digest: profile.image_digest.clone(),
    };
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
        owner_window_label: "main".into(),
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

fn sidecars(data: DataPaths) -> Sidecars {
    let lance_home = data.lance_language_model_home();
    Sidecars::new(SidecarConfig {
        jobworkerp_bin: "/bin/true".into(),
        memories_bin: "/bin/true".into(),
        memories_db_migrate_bin: "/bin/true".into(),
        startup_database_migration_enabled: false,
        conductor_bin: "/bin/true".into(),
        data,
        worker_yaml_paths: Vec::new(),
        issue_tracker_server_bin: None,
        function_set_yaml_paths: Vec::new(),
        reflection_dispatch_enabled: false,
        auto_embedding_enabled: false,
        workflows_dir: None,
        lance_language_model_home: lance_home,
        lindera_dict_staged: false,
        llm_model: None,
        llm_hf_repo: None,
        llm_ctx_size: None,
        llm_kv_cache_type: None,
        env_file: None,
    })
}

#[tokio::test]
async fn managed_authority_fails_closed_when_the_local_tracker_is_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let data = DataPaths::with_root(temp.path().join("app-data"));
    let authority = ManagedNativeSessionAuthority::new(
        Arc::new(sidecars(data)),
        Arc::new(IssueTrackerAvailability::default()),
        Arc::new(
            lookback_tauri_lib::coding_agent::settings::CodingAgentSettingsStore::new(
                temp.path().join("app-data/coding-agent/settings.json"),
            ),
        ),
    );

    let error = authority
        .read_current(&context(temp.path()))
        .await
        .expect_err("unavailable local canonical authority must block native admission");

    assert!(matches!(error, NativeAuthorityError::Unavailable));
    assert_eq!(
        error.to_string(),
        "canonical native session authority is unavailable"
    );
    assert!(
        !error
            .to_string()
            .contains(temp.path().to_string_lossy().as_ref())
    );
}

#[tokio::test]
async fn preparation_authority_fails_closed_without_a_connected_managed_tracker() {
    let temp = tempfile::tempdir().unwrap();
    let data = DataPaths::with_root(temp.path().join("app-data"));
    let authority = ManagedNativeSessionAuthority::new(
        Arc::new(sidecars(data)),
        Arc::new(IssueTrackerAvailability::default()),
        Arc::new(
            lookback_tauri_lib::coding_agent::settings::CodingAgentSettingsStore::new(
                temp.path().join("app-data/coding-agent/settings.json"),
            ),
        ),
    );

    let error = authority
        .read_for_preparation(
            "issue-42",
            "opencode-native",
            HostResourceLimits {
                cpu_millis: 4000,
                memory_mib: 8192,
                root_disk_mib: 32768,
                max_parallel_runs: 2,
            },
        )
        .await
        .expect_err("preparation must not fall back to a non-local tracker");

    assert!(matches!(error, NativeAuthorityError::Unavailable));
    assert_eq!(
        error.to_string(),
        "canonical native session authority is unavailable"
    );
}
