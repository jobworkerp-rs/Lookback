use std::{collections::BTreeMap, path::Path, sync::Arc, thread};

use coding_agent_executor::{
    BaseResolutionReceipt, PROTOCOL_VERSION, PreparedWorkspace, RepositoryId,
};
use lookback_tauri_lib::{
    coding_agent::{
        AgentProfile, AgentProfileSnapshot, CloneTransport, ForgeKind, HostResourceLimits,
        RepositoryHostConfig, RepositoryResolution, ResolvedRepository, SandboxResources,
        resolve_repository,
    },
    native_terminal::{
        CanonicalIssueSnapshot, NativeCancelClaim, NativeSessionContext, NativeSessionContextInput,
        NativeTerminalObservation, NativeTerminalPreview, NativeTerminalStore,
        NativeTerminalStoreError, SessionState, StartClaimOutcome,
        launch::{NativeAgentKind, NativeCliProfile, WorkspaceMountBinding, build_launch_plan},
    },
};
use sha2::{Digest, Sha256};

const IMAGE_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OPEN_CODE_VERSION: &str = "1.18.3";
const OWNER_WINDOW: &str = "main";

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

fn uuid(value: u8) -> String {
    format!("00000000-0000-4000-8000-{value:012x}")
}

fn make_context(
    base: &Path,
    session_id: &str,
    attempt_id: &str,
    preview_id: &str,
    owner_window_label: &str,
    edited_prompt: Option<&str>,
) -> NativeSessionContext {
    let repository = repository();
    let workspace_root = base.join("workspace-mount");
    std::fs::create_dir_all(&workspace_root).unwrap();
    let workspace_path = workspace_root.join("issue-42");
    std::fs::create_dir_all(&workspace_path).unwrap();
    let workspace_repository = RepositoryId {
        host: repository.identity.host.clone(),
        owner: repository.identity.organization.clone(),
        name: repository.identity.repository.clone(),
    };
    let base_commit = "b".repeat(40);
    let attempt_id = attempt_id.to_owned();
    let session_id = session_id.to_owned();
    let base_receipt = BaseResolutionReceipt {
        run_id: session_id.clone(),
        attempt_id: attempt_id.clone(),
        operation_id: uuid(90),
        repository: workspace_repository.clone(),
        base_branch: "main".into(),
        base_commit: base_commit.clone(),
        trusted_config_digest: "c".repeat(64),
        protocol_version: PROTOCOL_VERSION,
        executor_version: "0.2.0".into(),
    };
    let prepared_workspace = PreparedWorkspace {
        run_id: session_id.clone(),
        attempt_id: attempt_id.clone(),
        repository: workspace_repository,
        base_branch: "main".into(),
        base_commit,
        branch: "native/issue-42".into(),
        workspace_path: workspace_path.to_string_lossy().into_owned(),
    };
    let profile = profile(base, &repository);
    let issue = CanonicalIssueSnapshot::new(
        "issue-42",
        11,
        "Add terminal support",
        "Keep the exact body.\nSecond line.",
        vec!["first\ncriterion".into(), "second criterion".into()],
    )
    .unwrap();
    let cli_profile = NativeCliProfile {
        kind: NativeAgentKind::OpenCode,
        cli_version: OPEN_CODE_VERSION.into(),
        runtime_image_digest: profile.image_digest.clone(),
        expected_profile_command: profile.command.clone(),
    };
    let mount_binding = WorkspaceMountBinding {
        canonical_host_root: workspace_root,
        guest_root: "/workspace".into(),
    };
    let launch_plan = build_launch_plan(
        &profile,
        &cli_profile,
        &prepared_workspace,
        &mount_binding,
        &issue,
        edited_prompt,
    )
    .unwrap();
    let preview = NativeTerminalPreview::from_launch(
        preview_id,
        14,
        issue.revision(),
        Some(repository.identity.clone()),
        &launch_plan,
        None,
    )
    .unwrap();

    NativeSessionContext::new(NativeSessionContextInput {
        session_id,
        attempt_id,
        owner_window_label: owner_window_label.into(),
        canonical_issue: issue,
        repository,
        base_resolution_receipt: base_receipt,
        prepared_workspace,
        profile,
        cli_profile,
        workspace_mount_binding: mount_binding,
        launch_plan,
        preview,
    })
    .unwrap()
}

fn context_with_runtime_environment(
    context: &NativeSessionContext,
    runtime_environment: BTreeMap<String, String>,
) -> NativeSessionContext {
    let mut profile = context.profile().clone();
    profile.runtime_environment = runtime_environment;
    NativeSessionContext::new(NativeSessionContextInput {
        session_id: context.session_id().into(),
        attempt_id: context.attempt_id().into(),
        owner_window_label: context.owner_window_label().into(),
        canonical_issue: context.canonical_issue().clone(),
        repository: context.repository().clone(),
        base_resolution_receipt: context.base_resolution_receipt().clone(),
        prepared_workspace: context.prepared_workspace().clone(),
        profile,
        cli_profile: context.cli_profile().clone(),
        workspace_mount_binding: context.workspace_mount_binding().clone(),
        launch_plan: context.launch_plan().clone(),
        preview: context.preview().clone(),
    })
    .unwrap()
}

#[test]
fn legacy_context_without_runtime_environment_roundtrips_without_wire_changes() {
    let temp = tempfile::tempdir().unwrap();
    let context = make_context(
        temp.path(),
        &uuid(201),
        &uuid(202),
        &uuid(203),
        OWNER_WINDOW,
        None,
    );
    assert!(context.profile().runtime_environment.is_empty());
    let legacy_bytes = serde_json::to_vec(&context).unwrap();
    let legacy_wire: serde_json::Value = serde_json::from_slice(&legacy_bytes).unwrap();
    assert!(
        legacy_wire
            .get("profile")
            .and_then(serde_json::Value::as_object)
            .and_then(|profile| profile.get("runtime_environment"))
            .is_none()
    );
    let decoded: NativeSessionContext = serde_json::from_slice(&legacy_bytes).unwrap();

    assert!(decoded.profile().runtime_environment.is_empty());
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), legacy_bytes);
    assert_eq!(decoded.digest().unwrap(), context.digest().unwrap());
}

#[test]
fn runtime_environment_changes_the_native_context_digest() {
    let temp = tempfile::tempdir().unwrap();
    let context = make_context(
        temp.path(),
        &uuid(204),
        &uuid(205),
        &uuid(206),
        OWNER_WINDOW,
        None,
    );
    let configured = context_with_runtime_environment(
        &context,
        BTreeMap::from([
            ("CARGO_HOME".into(), "/home/codespace/.cargo".into()),
            ("RUSTUP_HOME".into(), "/home/codespace/.rustup".into()),
        ]),
    );

    assert_ne!(context.digest().unwrap(), configured.digest().unwrap());
}

fn save_preview(store: &NativeTerminalStore, context: &NativeSessionContext) {
    store
        .persist_preview(context.preview(), context.owner_window_label())
        .unwrap();
}

fn open_store(path: &Path) -> NativeTerminalStore {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            path.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    NativeTerminalStore::open(path).unwrap()
}

fn database_path(root: &Path) -> std::path::PathBuf {
    root.join("native-terminal.sqlite3")
}

fn rewrite_stored_preview(
    database: &Path,
    preview: &NativeTerminalPreview,
    command: Option<&str>,
    args: Option<&[String]>,
) {
    let connection = rusqlite::Connection::open(database).unwrap();
    let stored_json: Vec<u8> = connection
        .query_row(
            "SELECT preview_json FROM native_terminal_previews WHERE preview_id = ?1",
            [preview.preview_id()],
            |row| row.get(0),
        )
        .unwrap();
    let mut rewritten = String::from_utf8(stored_json).unwrap();
    if let Some(command) = command {
        let old = format!(
            "\"command\":{}",
            serde_json::to_string(preview.command()).unwrap()
        );
        let new = format!("\"command\":{}", serde_json::to_string(command).unwrap());
        assert!(rewritten.contains(&old));
        rewritten = rewritten.replacen(&old, &new, 1);
    }
    if let Some(args) = args {
        let old = format!(
            "\"args\":{}",
            serde_json::to_string(preview.args()).unwrap()
        );
        let new = format!("\"args\":{}", serde_json::to_string(args).unwrap());
        assert!(rewritten.contains(&old));
        rewritten = rewritten.replacen(&old, &new, 1);
    }

    let old_digest = format!("\"previewDigest\":\"{}\"", preview.preview_digest());
    let empty_digest = "\"previewDigest\":\"\"";
    assert!(rewritten.contains(&old_digest));
    rewritten = rewritten.replacen(&old_digest, empty_digest, 1);
    let mut hasher = Sha256::new();
    hasher.update(b"lookback/native-terminal/preview/v1\0");
    hasher.update(rewritten.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    let new_digest = format!("\"previewDigest\":\"{digest}\"");
    rewritten = rewritten.replacen(empty_digest, &new_digest, 1);
    connection
        .execute(
            "UPDATE native_terminal_previews
                SET preview_json = ?2, preview_digest = ?3
              WHERE preview_id = ?1",
            rusqlite::params![preview.preview_id(), rewritten.as_bytes(), digest],
        )
        .unwrap();
}

#[test]
fn native_session_start_is_durable_and_claimed_before_any_rpc() {
    let temp = tempfile::tempdir().unwrap();
    let database = database_path(temp.path());
    let context = make_context(
        temp.path(),
        &uuid(1),
        &uuid(2),
        &uuid(3),
        OWNER_WINDOW,
        None,
    );
    let context_digest = context.digest().unwrap();
    let store = open_store(&database);
    save_preview(&store, &context);

    let first = store.claim_start(context.clone(), OWNER_WINDOW).unwrap();
    let StartClaimOutcome::Claimed(record) = first else {
        panic!("the first caller owns the native start claim");
    };
    assert_eq!(record.state, SessionState::Starting);
    assert_eq!(record.native_job_id, None);
    assert_eq!(record.preview_digest, context.preview().preview_digest());
    assert_eq!(record.expected_settings_revision, 14);
    assert_eq!(record.expected_issue_revision, 11);
    assert!(matches!(
        store.get_session_for_window(context.session_id(), "secondary-window"),
        Err(NativeTerminalStoreError::OwnerMismatch)
    ));

    let retry = store.claim_start(context.clone(), OWNER_WINDOW).unwrap();
    let StartClaimOutcome::Existing(existing) = retry else {
        panic!("a duplicate must be a lookup, not another enqueue authorization");
    };
    assert_eq!(existing.state, SessionState::Starting);
    assert_eq!(existing.session_id, context.session_id());

    std::fs::remove_dir_all(&context.prepared_workspace().workspace_path).unwrap();
    drop(store);
    let reopened = open_store(&database);
    let recovered = reopened.get_session(context.session_id()).unwrap().unwrap();
    assert_eq!(recovered.state, SessionState::Starting);
    assert_eq!(recovered.context_digest, context_digest);
    assert_eq!(recovered.context.digest().unwrap(), context_digest);
    assert_eq!(recovered.context.initial_prompt(), context.initial_prompt());
}

#[test]
fn preview_owner_digest_and_context_are_immutable_start_preconditions() {
    let temp = tempfile::tempdir().unwrap();
    let store = open_store(&database_path(temp.path()));
    let context = make_context(
        temp.path(),
        &uuid(10),
        &uuid(11),
        &uuid(12),
        OWNER_WINDOW,
        None,
    );
    save_preview(&store, &context);

    let changed = make_context(
        temp.path(),
        &uuid(10),
        &uuid(11),
        &uuid(12),
        OWNER_WINDOW,
        Some("different initial prompt"),
    );
    assert!(matches!(
        store.persist_preview(changed.preview(), OWNER_WINDOW),
        Err(NativeTerminalStoreError::Conflict)
    ));

    assert!(matches!(
        store.persist_preview(context.preview(), "secondary-window"),
        Err(NativeTerminalStoreError::OwnerMismatch)
    ));
    assert!(matches!(
        store.claim_start(
            make_context(
                temp.path(),
                &uuid(10),
                &uuid(11),
                &uuid(12),
                OWNER_WINDOW,
                Some("different initial prompt"),
            ),
            OWNER_WINDOW,
        ),
        Err(NativeTerminalStoreError::Conflict)
    ));
    store.claim_start(context.clone(), OWNER_WINDOW).unwrap();
    assert!(matches!(
        store.claim_start(
            make_context(
                temp.path(),
                &uuid(14),
                &uuid(15),
                &uuid(12),
                OWNER_WINDOW,
                None,
            ),
            OWNER_WINDOW,
        ),
        Err(NativeTerminalStoreError::Conflict)
    ));

    assert!(matches!(
        store.claim_start(context, "secondary-window"),
        Err(NativeTerminalStoreError::OwnerMismatch)
    ));
}

#[test]
fn concurrent_cancel_claim_from_separate_connections_dispatches_delete_once() {
    let temp = tempfile::tempdir().unwrap();
    let database = database_path(temp.path());
    let store = Arc::new(open_store(&database));
    let context = make_context(
        temp.path(),
        &uuid(20),
        &uuid(21),
        &uuid(22),
        OWNER_WINDOW,
        None,
    );
    save_preview(&store, &context);
    store.claim_start(context.clone(), OWNER_WINDOW).unwrap();
    store
        .record_job_accepted(context.session_id(), 733)
        .unwrap();
    let second = Arc::new(open_store(&database));

    let left_store = Arc::clone(&store);
    let left_session = context.session_id().to_owned();
    let left = thread::spawn(move || {
        left_store
            .claim_cancel(&left_session, OWNER_WINDOW)
            .unwrap()
    });
    let right_store = Arc::clone(&second);
    let right_session = context.session_id().to_owned();
    let right = thread::spawn(move || {
        right_store
            .claim_cancel(&right_session, OWNER_WINDOW)
            .unwrap()
    });
    let outcomes = [left.join().unwrap(), right.join().unwrap()];

    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, NativeCancelClaim::Dispatch { job_id: 733 }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, NativeCancelClaim::AlreadyRequested))
            .count(),
        1
    );
    assert!(matches!(
        store.claim_cancel(context.session_id(), "secondary-window"),
        Err(NativeTerminalStoreError::OwnerMismatch)
    ));
}

#[test]
fn cancellation_requested_before_acceptance_is_dispatched_once_when_job_id_arrives() {
    let temp = tempfile::tempdir().unwrap();
    let store = open_store(&database_path(temp.path()));
    let context = make_context(
        temp.path(),
        &uuid(30),
        &uuid(31),
        &uuid(32),
        OWNER_WINDOW,
        None,
    );
    save_preview(&store, &context);
    store.claim_start(context.clone(), OWNER_WINDOW).unwrap();

    assert_eq!(
        store
            .claim_cancel(context.session_id(), OWNER_WINDOW)
            .unwrap(),
        NativeCancelClaim::PendingAcceptance
    );
    let accepted = store
        .record_job_accepted(context.session_id(), 734)
        .unwrap();
    assert_eq!(accepted.cancel_dispatch_job_id, Some(734));
    assert_eq!(
        store
            .claim_cancel(context.session_id(), OWNER_WINDOW)
            .unwrap(),
        NativeCancelClaim::AlreadyRequested
    );
}

#[test]
fn incomplete_terminal_facts_remain_unknown_and_real_exit_is_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let store = open_store(&database_path(temp.path()));
    let context = make_context(
        temp.path(),
        &uuid(40),
        &uuid(41),
        &uuid(42),
        OWNER_WINDOW,
        None,
    );
    save_preview(&store, &context);
    store.claim_start(context.clone(), OWNER_WINDOW).unwrap();
    store
        .record_job_accepted(context.session_id(), 735)
        .unwrap();

    let incomplete = store
        .record_terminal_observation(
            context.session_id(),
            NativeTerminalObservation {
                exit_code: Some(0),
                end_received: true,
                stream_eof: false,
                stream_error: false,
            },
        )
        .unwrap();
    assert_eq!(incomplete.state, SessionState::Unknown);
    assert_eq!(incomplete.exit_code, Some(0));
    assert!(!incomplete.stream_eof);

    let completed = store
        .record_terminal_observation(
            context.session_id(),
            NativeTerminalObservation {
                exit_code: Some(0),
                end_received: true,
                stream_eof: true,
                stream_error: false,
            },
        )
        .unwrap();
    assert_eq!(completed.state, SessionState::Closed);
    assert_eq!(completed.exit_code, Some(0));
    assert!(completed.end_received);
    assert!(completed.stream_eof);
    assert!(!completed.stream_error);
    assert!(store.unresolved_profile_ids().unwrap().is_empty());

    let no_job_id = make_context(
        temp.path(),
        &uuid(43),
        &uuid(44),
        &uuid(45),
        OWNER_WINDOW,
        None,
    );
    save_preview(&store, &no_job_id);
    store.claim_start(no_job_id.clone(), OWNER_WINDOW).unwrap();
    let unbound_exit = store
        .record_terminal_observation(
            no_job_id.session_id(),
            NativeTerminalObservation {
                exit_code: Some(0),
                end_received: true,
                stream_eof: true,
                stream_error: false,
            },
        )
        .unwrap();
    assert_eq!(unbound_exit.state, SessionState::Unknown);
    assert_eq!(unbound_exit.native_job_id, None);
}

#[test]
fn requested_cancel_is_terminal_only_after_real_exit_end_and_eof() {
    let temp = tempfile::tempdir().unwrap();
    let store = open_store(&database_path(temp.path()));
    let context = make_context(
        temp.path(),
        &uuid(50),
        &uuid(51),
        &uuid(52),
        OWNER_WINDOW,
        None,
    );
    save_preview(&store, &context);
    store.claim_start(context.clone(), OWNER_WINDOW).unwrap();
    store
        .record_job_accepted(context.session_id(), 736)
        .unwrap();
    store
        .claim_cancel(context.session_id(), OWNER_WINDOW)
        .unwrap();

    let unknown = store
        .record_terminal_observation(
            context.session_id(),
            NativeTerminalObservation {
                exit_code: Some(143),
                end_received: true,
                stream_eof: true,
                stream_error: true,
            },
        )
        .unwrap();
    assert_eq!(unknown.state, SessionState::Unknown);
    assert_eq!(unknown.exit_code, Some(143));
    assert!(unknown.stream_error);

    let still_unknown = store
        .record_terminal_observation(
            context.session_id(),
            NativeTerminalObservation {
                exit_code: Some(143),
                end_received: true,
                stream_eof: true,
                stream_error: false,
            },
        )
        .unwrap();
    assert_eq!(still_unknown.state, SessionState::Unknown);
    assert!(still_unknown.stream_error);

    let completed_cancel = make_context(
        temp.path(),
        &uuid(53),
        &uuid(54),
        &uuid(55),
        OWNER_WINDOW,
        None,
    );
    save_preview(&store, &completed_cancel);
    store
        .claim_start(completed_cancel.clone(), OWNER_WINDOW)
        .unwrap();
    store
        .record_job_accepted(completed_cancel.session_id(), 738)
        .unwrap();
    store
        .claim_cancel(completed_cancel.session_id(), OWNER_WINDOW)
        .unwrap();
    let cancelled = store
        .record_terminal_observation(
            completed_cancel.session_id(),
            NativeTerminalObservation {
                exit_code: Some(143),
                end_received: true,
                stream_eof: true,
                stream_error: false,
            },
        )
        .unwrap();
    assert_eq!(cancelled.state, SessionState::Cancelled);
    assert_eq!(cancelled.exit_code, Some(143));
    assert_eq!(
        store
            .claim_cancel(completed_cancel.session_id(), OWNER_WINDOW)
            .unwrap(),
        NativeCancelClaim::Terminal
    );
}

#[test]
fn restart_marks_nonterminal_sessions_unknown_without_losing_known_native_job_id() {
    let temp = tempfile::tempdir().unwrap();
    let database = database_path(temp.path());
    let context = make_context(
        temp.path(),
        &uuid(60),
        &uuid(61),
        &uuid(62),
        OWNER_WINDOW,
        None,
    );
    let store = open_store(&database);
    save_preview(&store, &context);
    store.claim_start(context.clone(), OWNER_WINDOW).unwrap();
    store
        .record_job_accepted(context.session_id(), 737)
        .unwrap();
    drop(store);

    let reopened = open_store(&database);
    assert_eq!(reopened.mark_unresolved_unknown().unwrap(), 1);
    let unknown = reopened.get_session(context.session_id()).unwrap().unwrap();
    assert_eq!(unknown.state, SessionState::Unknown);
    assert_eq!(unknown.native_job_id, Some(737));
    assert_eq!(
        reopened.unresolved_profile_ids().unwrap(),
        vec!["opencode-native"]
    );
    assert_eq!(reopened.mark_unresolved_unknown().unwrap(), 0);
}

#[test]
fn unresolved_profile_lookup_rejects_a_tampered_profile_id_instead_of_freezing_the_wrong_profile() {
    let temp = tempfile::tempdir().unwrap();
    let database = database_path(temp.path());
    let context = make_context(
        temp.path(),
        &uuid(80),
        &uuid(81),
        &uuid(82),
        OWNER_WINDOW,
        None,
    );
    {
        let store = open_store(&database);
        save_preview(&store, &context);
        store.claim_start(context.clone(), OWNER_WINDOW).unwrap();
    }

    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE native_terminal_sessions SET profile_id = 'tampered-profile' WHERE session_id = ?1",
            [context.session_id()],
        )
        .unwrap();
    drop(connection);

    let store = open_store(&database);
    assert!(matches!(
        store.unresolved_profile_ids(),
        Err(NativeTerminalStoreError::CorruptStore)
    ));
}

#[test]
fn unresolved_profile_lookup_rejects_terminal_state_without_complete_terminal_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let database = database_path(temp.path());
    let context = make_context(
        temp.path(),
        &uuid(83),
        &uuid(84),
        &uuid(85),
        OWNER_WINDOW,
        None,
    );
    {
        let store = open_store(&database);
        save_preview(&store, &context);
        store.claim_start(context.clone(), OWNER_WINDOW).unwrap();
        store
            .record_job_accepted(context.session_id(), 739)
            .unwrap();
    }

    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE native_terminal_sessions SET state = 'closed' WHERE session_id = ?1",
            [context.session_id()],
        )
        .unwrap();
    drop(connection);

    let store = open_store(&database);
    assert!(matches!(
        store.unresolved_profile_ids(),
        Err(NativeTerminalStoreError::CorruptStore)
    ));
}

#[test]
fn stored_native_context_rejects_unknown_nested_json_fields() {
    let temp = tempfile::tempdir().unwrap();
    let database = database_path(temp.path());
    let context = make_context(
        temp.path(),
        &uuid(86),
        &uuid(87),
        &uuid(88),
        OWNER_WINDOW,
        None,
    );
    {
        let store = open_store(&database);
        save_preview(&store, &context);
        store.claim_start(context.clone(), OWNER_WINDOW).unwrap();
    }

    let connection = rusqlite::Connection::open(&database).unwrap();
    let stored_json: Vec<u8> = connection
        .query_row(
            "SELECT context_json FROM native_terminal_sessions WHERE session_id = ?1",
            [context.session_id()],
            |row| row.get(0),
        )
        .unwrap();
    let mut stored_value: serde_json::Value = serde_json::from_slice(&stored_json).unwrap();
    stored_value["profile"]["untrusted_extra"] = serde_json::json!({"ignored": true});
    connection
        .execute(
            "UPDATE native_terminal_sessions SET context_json = ?2 WHERE session_id = ?1",
            rusqlite::params![
                context.session_id(),
                serde_json::to_vec(&stored_value).unwrap()
            ],
        )
        .unwrap();
    drop(connection);

    let store = open_store(&database);
    assert!(matches!(
        store.get_session(context.session_id()),
        Err(NativeTerminalStoreError::CorruptStore)
    ));
}

#[test]
fn failed_initial_schema_creation_rolls_back_its_partial_ddl() {
    let temp = tempfile::tempdir().unwrap();
    let database = database_path(temp.path());
    drop(open_store(&database));

    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "DROP INDEX native_terminal_sessions_profile_state;
             DROP TABLE native_terminal_sessions;
             DROP TABLE native_terminal_previews;
             CREATE TABLE native_terminal_sessions (preserved INTEGER);
             PRAGMA user_version = 0;",
        )
        .unwrap();
    drop(connection);

    assert!(NativeTerminalStore::open(&database).is_err());
    let connection = rusqlite::Connection::open(&database).unwrap();
    let has_partial_previews: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'native_terminal_previews')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let preserved_sessions: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'native_terminal_sessions')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!has_partial_previews);
    assert!(preserved_sessions);
}

#[test]
fn repeated_job_acceptance_does_not_resolve_unknown_state_or_reopen_a_closed_session() {
    let temp = tempfile::tempdir().unwrap();
    let store = open_store(&database_path(temp.path()));
    let unknown = make_context(
        temp.path(),
        &uuid(89),
        &uuid(91),
        &uuid(92),
        OWNER_WINDOW,
        None,
    );
    save_preview(&store, &unknown);
    store.claim_start(unknown.clone(), OWNER_WINDOW).unwrap();
    store.mark_start_unknown(unknown.session_id()).unwrap();
    assert_eq!(
        store
            .claim_cancel(unknown.session_id(), OWNER_WINDOW)
            .unwrap(),
        NativeCancelClaim::PendingAcceptance
    );

    let first_acceptance = store
        .record_job_accepted(unknown.session_id(), 740)
        .unwrap();
    assert_eq!(first_acceptance.cancel_dispatch_job_id, Some(740));
    assert_eq!(
        store
            .get_session(unknown.session_id())
            .unwrap()
            .unwrap()
            .state,
        SessionState::Unknown
    );
    let duplicate = store
        .record_job_accepted(unknown.session_id(), 740)
        .unwrap();
    assert_eq!(duplicate.cancel_dispatch_job_id, None);
    assert_eq!(
        store
            .get_session(unknown.session_id())
            .unwrap()
            .unwrap()
            .state,
        SessionState::Unknown
    );
    assert!(matches!(
        store.record_job_accepted(unknown.session_id(), 741),
        Err(NativeTerminalStoreError::Conflict)
    ));

    let closed = make_context(
        temp.path(),
        &uuid(93),
        &uuid(94),
        &uuid(95),
        OWNER_WINDOW,
        None,
    );
    save_preview(&store, &closed);
    store.claim_start(closed.clone(), OWNER_WINDOW).unwrap();
    store.record_job_accepted(closed.session_id(), 742).unwrap();
    store
        .record_terminal_observation(
            closed.session_id(),
            NativeTerminalObservation {
                exit_code: Some(0),
                end_received: true,
                stream_eof: true,
                stream_error: false,
            },
        )
        .unwrap();

    let duplicate = store.record_job_accepted(closed.session_id(), 742).unwrap();
    assert_eq!(duplicate.cancel_dispatch_job_id, None);
    let record = store.get_session(closed.session_id()).unwrap().unwrap();
    assert_eq!(record.state, SessionState::Closed);
    assert_eq!(record.native_job_id, Some(742));
    assert_eq!(record.exit_code, Some(0));
    assert!(record.end_received);
    assert!(record.stream_eof);
    assert!(matches!(
        store.record_job_accepted(closed.session_id(), 743),
        Err(NativeTerminalStoreError::Conflict)
    ));
}

#[test]
fn late_job_acceptance_completes_only_cleanly_observed_terminal_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let store = open_store(&database_path(temp.path()));
    let closed = make_context(
        temp.path(),
        &uuid(96),
        &uuid(97),
        &uuid(98),
        OWNER_WINDOW,
        None,
    );
    save_preview(&store, &closed);
    store.claim_start(closed.clone(), OWNER_WINDOW).unwrap();
    let observed_before_acceptance = store
        .record_terminal_observation(
            closed.session_id(),
            NativeTerminalObservation {
                exit_code: Some(23),
                end_received: true,
                stream_eof: true,
                stream_error: false,
            },
        )
        .unwrap();
    assert_eq!(observed_before_acceptance.state, SessionState::Unknown);
    assert_eq!(
        store.unresolved_profile_ids().unwrap(),
        vec!["opencode-native"]
    );

    store.record_job_accepted(closed.session_id(), 744).unwrap();
    let completed = store.get_session(closed.session_id()).unwrap().unwrap();
    assert_eq!(completed.state, SessionState::Closed);
    assert_eq!(completed.native_job_id, Some(744));
    assert_eq!(completed.exit_code, Some(23));
    assert!(completed.end_received);
    assert!(completed.stream_eof);
    assert!(!completed.stream_error);
    assert!(store.unresolved_profile_ids().unwrap().is_empty());

    let failed_stream = make_context(
        temp.path(),
        &uuid(99),
        &uuid(100),
        &uuid(101),
        OWNER_WINDOW,
        None,
    );
    save_preview(&store, &failed_stream);
    store
        .claim_start(failed_stream.clone(), OWNER_WINDOW)
        .unwrap();
    store
        .record_job_accepted(failed_stream.session_id(), 745)
        .unwrap();
    let observed_error = store
        .record_terminal_observation(
            failed_stream.session_id(),
            NativeTerminalObservation {
                exit_code: Some(9),
                end_received: true,
                stream_eof: true,
                stream_error: true,
            },
        )
        .unwrap();
    assert_eq!(observed_error.state, SessionState::Unknown);
    assert_eq!(
        store
            .record_job_accepted(failed_stream.session_id(), 745)
            .unwrap()
            .cancel_dispatch_job_id,
        None
    );
    let still_unknown = store
        .get_session(failed_stream.session_id())
        .unwrap()
        .unwrap();
    assert_eq!(still_unknown.state, SessionState::Unknown);
    assert_eq!(still_unknown.exit_code, Some(9));
    assert!(still_unknown.stream_error);

    let cancelled = make_context(
        temp.path(),
        &uuid(102),
        &uuid(103),
        &uuid(104),
        OWNER_WINDOW,
        None,
    );
    save_preview(&store, &cancelled);
    store.claim_start(cancelled.clone(), OWNER_WINDOW).unwrap();
    assert_eq!(
        store
            .claim_cancel(cancelled.session_id(), OWNER_WINDOW)
            .unwrap(),
        NativeCancelClaim::PendingAcceptance
    );
    let cancel_observation = store
        .record_terminal_observation(
            cancelled.session_id(),
            NativeTerminalObservation {
                exit_code: Some(143),
                end_received: true,
                stream_eof: true,
                stream_error: false,
            },
        )
        .unwrap();
    assert_eq!(cancel_observation.state, SessionState::Unknown);
    let acceptance = store
        .record_job_accepted(cancelled.session_id(), 746)
        .unwrap();
    assert_eq!(acceptance.cancel_dispatch_job_id, Some(746));
    assert_eq!(
        store
            .record_job_accepted(cancelled.session_id(), 746)
            .unwrap()
            .cancel_dispatch_job_id,
        None
    );
    let cancelled_record = store.get_session(cancelled.session_id()).unwrap().unwrap();
    assert_eq!(cancelled_record.state, SessionState::Cancelled);
    assert_eq!(cancelled_record.native_job_id, Some(746));
    assert_eq!(cancelled_record.exit_code, Some(143));
    assert!(cancelled_record.end_received);
    assert!(cancelled_record.stream_eof);
    assert!(!cancelled_record.stream_error);
}

#[test]
fn preview_reads_reject_oversized_raw_json_before_deserializing() {
    let temp = tempfile::tempdir().unwrap();
    let database = database_path(temp.path());
    let context = make_context(
        temp.path(),
        &uuid(105),
        &uuid(106),
        &uuid(107),
        OWNER_WINDOW,
        None,
    );
    let store = open_store(&database);
    save_preview(&store, &context);

    let connection = rusqlite::Connection::open(&database).unwrap();
    let stored_json: Vec<u8> = connection
        .query_row(
            "SELECT preview_json FROM native_terminal_previews WHERE preview_id = ?1",
            [context.preview().preview_id()],
            |row| row.get(0),
        )
        .unwrap();
    let mut oversized_json = stored_json;
    oversized_json.resize(
        lookback_tauri_lib::native_terminal::MAX_NATIVE_PREVIEW_BYTES + 1,
        b' ',
    );
    connection
        .execute(
            "UPDATE native_terminal_previews SET preview_json = ?2 WHERE preview_id = ?1",
            rusqlite::params![context.preview().preview_id(), oversized_json],
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        store.get_preview(context.preview().preview_id(), OWNER_WINDOW),
        Err(NativeTerminalStoreError::CorruptStore)
    ));
    assert!(matches!(
        store.claim_start(context, OWNER_WINDOW),
        Err(NativeTerminalStoreError::CorruptStore)
    ));
}

#[test]
fn preview_reads_accept_legacy_standalone_args_but_reject_unsupported_programs() {
    let temp = tempfile::tempdir().unwrap();
    let database = database_path(temp.path());
    let legacy_context = make_context(
        temp.path(),
        &uuid(108),
        &uuid(109),
        &uuid(110),
        OWNER_WINDOW,
        None,
    );
    let unsupported_context = make_context(
        temp.path(),
        &uuid(111),
        &uuid(112),
        &uuid(113),
        OWNER_WINDOW,
        None,
    );
    let store = open_store(&database);
    save_preview(&store, &legacy_context);
    save_preview(&store, &unsupported_context);

    let legacy_args = vec![
        "--standalone".to_owned(),
        legacy_context.preview().args()[0].clone(),
    ];
    rewrite_stored_preview(
        &database,
        legacy_context.preview(),
        None,
        Some(&legacy_args),
    );
    let restored = store
        .get_preview(legacy_context.preview().preview_id(), OWNER_WINDOW)
        .unwrap()
        .expect("legacy standalone previews remain readable");
    assert_eq!(restored.command(), "opencode");
    assert_eq!(restored.args(), legacy_args);
    assert_ne!(
        restored.preview_digest(),
        legacy_context.preview().preview_digest()
    );

    rewrite_stored_preview(
        &database,
        unsupported_context.preview(),
        Some("unsupported-agent"),
        None,
    );
    assert!(matches!(
        store.get_preview(unsupported_context.preview().preview_id(), OWNER_WINDOW),
        Err(NativeTerminalStoreError::CorruptStore)
    ));
}

#[cfg(unix)]
#[test]
fn native_database_rejects_symlink_and_nonprivate_parent() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let temp = tempfile::tempdir().unwrap();
    let store_root = temp.path().join("store-root");
    std::fs::create_dir(&store_root).unwrap();
    std::fs::set_permissions(&store_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = store_root.join("target.sqlite3");
    std::fs::write(&target, b"not opened through an alias").unwrap();
    let alias = store_root.join("native-terminal.sqlite3");
    symlink(&target, &alias).unwrap();
    assert!(NativeTerminalStore::open(&alias).is_err());

    let unsafe_parent = temp.path().join("unsafe");
    std::fs::create_dir(&unsafe_parent).unwrap();
    std::fs::set_permissions(&unsafe_parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(NativeTerminalStore::open(&unsafe_parent.join("native-terminal.sqlite3")).is_err());
}

#[cfg(unix)]
#[test]
fn native_database_and_sqlite_sidecars_remain_private() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let database = database_path(temp.path());
    let store = open_store(&database);
    let context = make_context(
        temp.path(),
        &uuid(70),
        &uuid(71),
        &uuid(72),
        OWNER_WINDOW,
        None,
    );
    save_preview(&store, &context);

    for path in [
        database.clone(),
        Path::new(&format!("{}-wal", database.display())).to_path_buf(),
        Path::new(&format!("{}-shm", database.display())).to_path_buf(),
    ] {
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            assert!(metadata.is_file());
            assert_eq!(metadata.permissions().mode() & 0o077, 0);
        }
    }
}
