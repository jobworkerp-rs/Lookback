#[cfg(unix)]
#[path = "fixtures/native_interactive_rpc.rs"]
mod jobworkerp_rpc;

#[cfg(unix)]
mod unix_tests {
    use super::jobworkerp_rpc;
    use std::{
        path::Path,
        path::PathBuf,
        sync::{Arc, Mutex as StdMutex},
    };

    use async_trait::async_trait;
    use coding_agent_executor::RepositoryId;
    use jobworkerp_client::proto::JobworkerpProto;
    use lookback_tauri_lib::{
        coding_agent::{
            AgentProfile, AgentProfileSnapshot, CredentialReference, ForgeKind, HostResourceLimits,
            PersistentMount, RepositoryIdentity, RepositoryProvenance, RepositorySelection,
            ResolvedRepository, SandboxResources,
        },
        jobworkerp::{JobworkerpHandle, coding_agent::HostWorkerContext},
        native_terminal::{
            CanonicalIssueSnapshot, NativeAuthorityError, NativeAuthoritySnapshot,
            NativePreparationAuthority, NativePreparationError, NativePreparationRuntime,
            NativeSessionAuthority, NativeSessionContext, NativeSessionPreparer,
            preparation::NativePrepareRequest,
        },
    };
    use serde::Serialize;
    use tokio::sync::Mutex;
    use tokio::sync::Notify;

    #[allow(dead_code)]
    mod package_fixture {
        include!("fixtures/native_terminal_package.rs");
    }
    mod preparation_rpc {
        include!("fixtures/native_terminal_preparation_rpc.rs");
    }

    const IMAGE_DIGEST: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ISSUE_ID: &str = "issue-42";
    const PROFILE_ID: &str = "opencode-native";

    fn host_limits() -> HostResourceLimits {
        HostResourceLimits {
            cpu_millis: 4000,
            memory_mib: 8192,
            root_disk_mib: 32768,
            max_parallel_runs: 2,
        }
    }

    fn repository(id: &RepositoryId) -> ResolvedRepository {
        let identity =
            RepositoryIdentity::new(&id.host, id.owner.clone(), id.name.clone()).unwrap();
        ResolvedRepository {
            identity,
            forge: ForgeKind::GitHub,
            api_base_url: format!("https://{}/api/v3", id.host),
            clone_url: format!("https://{}/{}/{}.git", id.host, id.owner, id.name),
            read_credential_ref: None,
            publication_credential_ref: None,
            provenance: RepositoryProvenance {
                source_labels: vec![format!("repo:{}/{}/{}", id.host, id.owner, id.name)],
                selection: RepositorySelection::SourceLabel,
                approved_by: None,
                approved_at_ms: None,
            },
        }
    }

    fn profile(root: &Path, repository: &ResolvedRepository) -> AgentProfileSnapshot {
        let image_ref = format!("registry.example/agents/opencode@{IMAGE_DIGEST}");
        AgentProfile {
            profile_id: PROFILE_ID.into(),
            version: 4,
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
        .validate_and_snapshot(&host_limits())
        .unwrap()
    }

    fn authority_snapshot(root: &Path, id: &RepositoryId) -> NativeAuthoritySnapshot {
        let repository = repository(id);
        NativeAuthoritySnapshot::from_host(
            CanonicalIssueSnapshot::new(
                ISSUE_ID,
                42,
                "Preserve the canonical title",
                "The latest authoritative Issue body.",
                vec!["Keep the interactive shell attached.".into()],
            )
            .unwrap(),
            repository.clone(),
            profile(root, &repository),
            19,
        )
    }

    fn native_request<'a>(
        base_branch: &'a str,
        edited_prompt: Option<&'a str>,
    ) -> NativePrepareRequest<'a> {
        NativePrepareRequest {
            issue_id: ISSUE_ID,
            profile_id: PROFILE_ID,
            base_branch,
            edited_prompt,
        }
    }

    fn snapshot_with_persistent_mount(
        mut snapshot: NativeAuthoritySnapshot,
        host_path: &Path,
        guest_path: &str,
    ) -> NativeAuthoritySnapshot {
        snapshot
            .profile
            .canonical_mount_allowed_roots
            .push(host_path.to_path_buf());
        snapshot.profile.persistent_mounts = vec![PersistentMount {
            host_path: host_path.to_path_buf(),
            guest_path: guest_path.to_owned(),
            read_only: true,
            executable: false,
        }];
        snapshot.profile.validate_snapshot().unwrap();
        snapshot
    }

    #[derive(Clone)]
    struct TestAuthority {
        snapshot: Arc<StdMutex<NativeAuthoritySnapshot>>,
        read_count: Arc<std::sync::atomic::AtomicUsize>,
        requests: Arc<StdMutex<Vec<(String, String, HostResourceLimits)>>>,
        replace_on_final_read: Arc<StdMutex<Option<NativeAuthoritySnapshot>>>,
        pause_read: Option<(usize, ReadGate)>,
    }

    #[derive(Clone)]
    struct ReadGate {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl ReadGate {
        fn new() -> Self {
            Self {
                entered: Arc::new(Notify::new()),
                release: Arc::new(Notify::new()),
            }
        }
    }

    impl TestAuthority {
        fn new(snapshot: NativeAuthoritySnapshot) -> Self {
            Self {
                snapshot: Arc::new(StdMutex::new(snapshot)),
                read_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                requests: Arc::new(StdMutex::new(Vec::new())),
                replace_on_final_read: Arc::new(StdMutex::new(None)),
                pause_read: None,
            }
        }

        fn changing_on_final_read(
            initial: NativeAuthoritySnapshot,
            changed: NativeAuthoritySnapshot,
        ) -> Self {
            let authority = Self::new(initial);
            *authority.replace_on_final_read.lock().unwrap() = Some(changed);
            authority
        }

        fn pausing_on_read(
            snapshot: NativeAuthoritySnapshot,
            read_number: usize,
        ) -> (Self, ReadGate) {
            let gate = ReadGate::new();
            let mut authority = Self::new(snapshot);
            authority.pause_read = Some((read_number, gate.clone()));
            (authority, gate)
        }

        fn replace_snapshot(&self, snapshot: NativeAuthoritySnapshot) {
            *self.snapshot.lock().unwrap() = snapshot;
        }

        fn requests(&self) -> Vec<(String, String, HostResourceLimits)> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl NativePreparationAuthority for TestAuthority {
        async fn read_for_preparation(
            &self,
            issue_id: &str,
            profile_id: &str,
            host_limits: HostResourceLimits,
        ) -> Result<NativeAuthoritySnapshot, NativeAuthorityError> {
            let read_number = self
                .read_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.requests.lock().unwrap().push((
                issue_id.to_owned(),
                profile_id.to_owned(),
                host_limits,
            ));
            if let Some((paused_read, gate)) = self.pause_read.as_ref()
                && read_number == *paused_read
            {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            if read_number > 0
                && let Some(changed) = self.replace_on_final_read.lock().unwrap().take()
            {
                *self.snapshot.lock().unwrap() = changed;
            }
            let snapshot = self.snapshot.lock().unwrap().clone();
            Ok(snapshot)
        }
    }

    #[async_trait]
    impl NativeSessionAuthority for TestAuthority {
        async fn read_current(
            &self,
            _expected: &NativeSessionContext,
        ) -> Result<NativeAuthoritySnapshot, NativeAuthorityError> {
            Ok(self.snapshot.lock().unwrap().clone())
        }
    }

    fn host_worker_context(root: &Path) -> HostWorkerContext {
        let runtime_root = root.join("native-runtime-fixture");
        let sdk_home = runtime_root.join("sdk/microsandbox");
        let backend_config_dir = runtime_root.join("backend");
        std::fs::create_dir_all(&sdk_home).unwrap();
        std::fs::create_dir_all(&backend_config_dir).unwrap();
        let backend_config_path = backend_config_dir.join("config.json");
        std::fs::write(&backend_config_path, b"{}\n").unwrap();
        HostWorkerContext {
            microsandbox_sdk_home: sdk_home.canonicalize().unwrap(),
            backend_config_path: backend_config_path.canonicalize().unwrap(),
        }
    }

    fn runtime(root: &Path) -> NativePreparationRuntime {
        NativePreparationRuntime::new(host_worker_context(root), host_limits()).unwrap()
    }

    async fn configure_native_repository(
        fixture: &mut package_fixture::NativeFixture,
        repository: &RepositoryId,
    ) -> lookback_tauri_lib::native_terminal::bootstrap::ConfiguredNativeRepository {
        preparation_rpc::install_preparation_executor(fixture);
        let selection = fixture.selection().unwrap();
        lookback_tauri_lib::native_terminal::bootstrap::configure_repository(
            &fixture.data_paths,
            &selection,
            repository,
        )
        .await
        .unwrap()
    }

    async fn start_preparer(
        configured: lookback_tauri_lib::native_terminal::bootstrap::ConfiguredNativeRepository,
        authority: TestAuthority,
        host_root: &Path,
    ) -> (
        NativeSessionPreparer<TestAuthority>,
        jobworkerp_rpc::RpcFixture,
        jobworkerp_rpc::TestServer,
        Arc<Mutex<()>>,
        PathBuf,
    ) {
        let database_path = configured.session_store().database_path().to_path_buf();
        let fixture_rpc = jobworkerp_rpc::RpcFixture::default();
        fixture_rpc.enable_preparation_workflow();
        let server = jobworkerp_rpc::start_server(fixture_rpc.clone()).await;
        let jobworkerp = JobworkerpHandle::connect(&server.url).await.unwrap();
        let shared_admission = Arc::new(Mutex::new(()));
        let preparer = NativeSessionPreparer::new(
            configured,
            Arc::new(authority.clone()),
            jobworkerp,
            runtime(host_root),
            Arc::clone(&shared_admission),
        );
        (
            preparer,
            fixture_rpc,
            server,
            shared_admission,
            database_path,
        )
    }

    async fn preparer(
        fixture: &mut package_fixture::NativeFixture,
        configured_repository: RepositoryId,
        current_repository: RepositoryId,
    ) -> (
        NativeSessionPreparer<TestAuthority>,
        jobworkerp_rpc::RpcFixture,
        jobworkerp_rpc::TestServer,
        TestAuthority,
        Arc<Mutex<()>>,
        PathBuf,
    ) {
        let configured = configure_native_repository(fixture, &configured_repository).await;
        let snapshot = authority_snapshot(configured.workspace_root(), &current_repository);
        let authority = TestAuthority::new(snapshot);
        let (preparer, fixture_rpc, server, admission, database_path) =
            start_preparer(configured, authority.clone(), fixture.temp.path()).await;
        (
            preparer,
            fixture_rpc,
            server,
            authority,
            admission,
            database_path,
        )
    }

    fn stored_preview_count(database_path: &Path) -> i64 {
        let connection = rusqlite::Connection::open(database_path).unwrap();
        connection
            .query_row("SELECT COUNT(*) FROM native_terminal_previews", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn native_workers(
        rpc: &jobworkerp_rpc::RpcFixture,
    ) -> Vec<jobworkerp_client::jobworkerp::data::WorkerData> {
        rpc.created_workers()
            .into_iter()
            .filter(|worker| !worker.name.starts_with("lookback.coding-agent.prepare."))
            .collect()
    }

    #[tokio::test]
    async fn automatic_preparation_returns_a_run_worker_without_staging_a_terminal() {
        let mut package = package_fixture::NativeFixture::new();
        let repository =
            package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
        let (preparer, rpc, _server, _, _, _) =
            preparer(&mut package, repository.clone(), repository).await;
        let prepared = preparer
            .prepare_automatic(native_request("", None))
            .await
            .unwrap();
        assert_eq!(prepared.worker.using(), "run");
        assert!(
            prepared
                .worker
                .worker_name()
                .starts_with("lookback.coding-agent.one-shot.")
        );
        assert_eq!(
            prepared.base_resolution.base_commit,
            prepared.workspace.base_commit
        );
        assert!(
            prepared
                .workspace
                .branch
                .starts_with("agent/issue-issue-42-")
        );
        assert_eq!(prepared.authority.canonical_issue.issue_id(), ISSUE_ID);
        assert_eq!(rpc.counts().enqueue_for_stream_calls, 2);
        assert_eq!(rpc.counts().enqueue_with_client_stream_calls, 0);
        assert!(
            preparer
                .manager()
                .recover_for_window("main")
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn automatic_preparation_rejects_authority_changed_during_workspace_preparation() {
        let mut package = package_fixture::NativeFixture::new();
        let repository =
            package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
        let configured = configure_native_repository(&mut package, &repository).await;
        let snapshot = authority_snapshot(configured.workspace_root(), &repository);
        let authority = TestAuthority::new(snapshot.clone());
        let mut changed = snapshot;
        changed.settings_revision += 1;
        *authority.replace_on_final_read.lock().unwrap() = Some(changed);
        let (preparer, rpc, _server, _, _) =
            start_preparer(configured, authority, package.temp.path()).await;
        assert!(matches!(
            preparer.prepare_automatic(native_request("", None)).await,
            Err(NativePreparationError::AuthorityChanged)
        ));
        assert_eq!(rpc.counts().enqueue_with_client_stream_calls, 0);
        assert!(
            preparer
                .manager()
                .recover_for_window("main")
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn default_base_is_resolved_by_preparation_workflow() {
        let mut package = package_fixture::NativeFixture::new();
        let repository =
            package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
        let (preparer, rpc, _server, _, _, _) =
            preparer(&mut package, repository.clone(), repository.clone()).await;
        let preview = preparer
            .prepare("main", native_request("", None))
            .await
            .unwrap();
        let log = package
            .configured_trusted_config(&repository)
            .state_root
            .join("native-preparation-fixture.log");
        assert_eq!(
            std::fs::read_to_string(log)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            [
                "resolve_base ",
                "prepare main bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ]
        );
        assert!(preview.working_directory().starts_with("/workspace/"));
        assert_eq!(rpc.counts().enqueue_for_stream_calls, 2);
        assert_eq!(rpc.counts().enqueue_with_client_stream_calls, 0);
    }

    #[tokio::test]
    async fn preparation_supports_codex_and_claude_profiles_with_empty_version_hints() {
        for (command, flags) in [
            (
                vec!["codex".to_owned(), "exec".to_owned()],
                vec!["--no-daemon", "--"],
            ),
            (vec!["claude".to_owned(), "-p".to_owned()], vec!["--"]),
        ] {
            let mut package = package_fixture::NativeFixture::new();
            let repository =
                package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
            let (preparer, rpc, _server, authority, _, _) =
                preparer(&mut package, repository.clone(), repository).await;
            let mut snapshot = authority.snapshot.lock().unwrap().clone();
            snapshot.profile.command = command.clone();
            *authority.snapshot.lock().unwrap() = snapshot;
            let prompt = "--literal\n日本語 '$()'";
            let preview = preparer
                .prepare("main", native_request("main", Some(prompt)))
                .await
                .unwrap();
            assert_eq!(preview.command(), command[0]);
            assert!(preview.expected_cli_version().is_empty());
            let mut expected: Vec<String> = flags.into_iter().map(str::to_owned).collect();
            expected.push(prompt.to_owned());
            assert_eq!(preview.args(), expected);
            assert_eq!(rpc.counts().enqueue_for_stream_calls, 2);
            assert_eq!(rpc.counts().enqueue_with_client_stream_calls, 0);
        }
    }

    #[tokio::test]
    async fn preparation_resolves_and_prepares_the_host_workspace_then_stages_a_preview() {
        let mut package = package_fixture::NativeFixture::new();
        let repository =
            package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
        let (preparer, worker_rpc, _server, authority, admission, _store_path) =
            preparer(&mut package, repository.clone(), repository.clone()).await;
        assert!(Arc::ptr_eq(
            &admission,
            &preparer.manager().admission_lock()
        ));
        assert_eq!(
            preparer.allowed_base_branches(),
            &["main".to_owned(), "develop".to_owned()]
        );
        let edited_prompt = "Investigate the failing test.\nKeep this text verbatim: λ";

        let preview = preparer
            .prepare("main", native_request("main", Some(edited_prompt)))
            .await
            .expect("a current approved Issue/profile should produce a preview");

        assert_eq!(preview.expected_settings_revision(), 19);
        assert_eq!(preview.expected_issue_revision(), 42);
        assert_eq!(preview.profile_id(), PROFILE_ID);
        assert!(preview.expected_cli_version().is_empty());
        assert_eq!(preview.safe_image_digest(), IMAGE_DIGEST);
        assert_eq!(preview.command(), "opencode");
        assert_eq!(preview.args(), &[format!("--prompt={edited_prompt}")]);
        assert!(preview.working_directory().starts_with("/workspace/"));
        let worker = preview
            .worker_binding()
            .expect("the prepared preview binds an app-verified native worker");
        assert!(worker.worker_id > 0);
        assert!(worker.runner_id > 0);
        assert_eq!(worker.image_digest, IMAGE_DIGEST);
        assert_eq!(worker.settings_digest.len(), 64);
        assert_eq!(native_workers(&worker_rpc).len(), 1);
        assert_eq!(worker_rpc.counts().enqueue_for_stream_calls, 2);
        assert_eq!(worker_rpc.counts().enqueue_with_client_stream_calls, 0);
        assert_eq!(
            authority.requests(),
            vec![
                (ISSUE_ID.into(), PROFILE_ID.into(), host_limits()),
                (ISSUE_ID.into(), PROFILE_ID.into(), host_limits()),
            ]
        );

        let second = preparer
            .prepare("main", native_request("develop", None))
            .await
            .expect("an explicitly approved non-main branch reuses the same worker");
        assert_ne!(preview.preview_id(), second.preview_id());
        assert_eq!(native_workers(&worker_rpc).len(), 1);
        assert_eq!(worker_rpc.counts().enqueue_for_stream_calls, 4);
        assert_eq!(worker_rpc.counts().enqueue_with_client_stream_calls, 0);
        let native_config = package.configured_trusted_config(&repository);
        assert_eq!(
            std::fs::read_to_string(
                native_config
                    .state_root
                    .join("native-preparation-fixture.log")
            )
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
            [
                "resolve_base main",
                "prepare main bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "resolve_base develop",
                "prepare develop bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ]
        );
    }

    #[tokio::test]
    async fn preparation_refuses_a_repository_that_differs_from_the_configured_native_partition() {
        let mut package = package_fixture::NativeFixture::new();
        let configured =
            package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
        let current =
            package_fixture::NativeFixture::repository(package_fixture::SECONDARY_REPOSITORY);
        let (preparer, worker_rpc, _server, _, _admission, _store_path) =
            preparer(&mut package, configured.clone(), current).await;
        let trusted_config = package.configured_trusted_config(&configured);
        let operation_log = trusted_config
            .state_root
            .join("native-preparation-fixture.log");

        let error = preparer
            .prepare("main", native_request("main", None))
            .await
            .expect_err("the latest Issue repository cannot cross native partitions");

        assert!(matches!(error, NativePreparationError::RepositoryMismatch));
        assert!(
            !operation_log.exists(),
            "the executor must not be called on mismatch"
        );
        assert!(worker_rpc.created_workers().is_empty());
        assert!(worker_rpc.job_requests().is_empty());
    }

    #[tokio::test]
    async fn preparation_rejects_profile_credential_references_before_any_worker_or_executor_operation()
     {
        let mut package = package_fixture::NativeFixture::new();
        let repository =
            package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
        let configured = configure_native_repository(&mut package, &repository).await;
        let workspace_root = configured.workspace_root().to_path_buf();
        let trusted_config = package.configured_trusted_config(&repository);
        let operation_log = trusted_config
            .state_root
            .join("native-preparation-fixture.log");
        let mut snapshot = authority_snapshot(&workspace_root, &repository);
        snapshot.profile.credential_reference =
            Some(CredentialReference::new("keychain:private-agent-secret-reference").unwrap());
        snapshot.profile.validate_snapshot().unwrap();
        let authority = TestAuthority::new(snapshot);
        let (preparer, worker_rpc, _server, _admission, store_path) =
            start_preparer(configured, authority, package.temp.path()).await;

        let error = preparer
            .prepare("main", native_request("main", None))
            .await
            .expect_err("unresolved host credential references must not be silently ignored");

        assert!(matches!(
            error,
            NativePreparationError::UnsupportedProfileCredentialReference
        ));
        assert!(!error.to_string().contains("private-agent-secret-reference"));
        assert!(!operation_log.exists());
        assert!(worker_rpc.created_workers().is_empty());
        assert!(worker_rpc.job_requests().is_empty());
        assert_eq!(stored_preview_count(&store_path), 0);
    }

    #[tokio::test]
    async fn preparation_rejects_an_unapproved_git_branch_before_creating_a_worker_or_calling_executor()
     {
        let mut package = package_fixture::NativeFixture::new();
        let repository =
            package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
        let (preparer, worker_rpc, _server, _authority, _admission, store_path) =
            preparer(&mut package, repository.clone(), repository.clone()).await;
        let trusted_config = package.configured_trusted_config(&repository);
        let operation_log = trusted_config
            .state_root
            .join("native-preparation-fixture.log");

        let error = preparer
            .prepare("main", native_request("--upload-pack=attacker", None))
            .await
            .expect_err("the branch must be an exact protected-policy entry");

        assert!(matches!(
            error,
            NativePreparationError::UnapprovedBaseBranch
        ));
        assert!(!operation_log.exists());
        assert!(worker_rpc.created_workers().is_empty());
        assert_eq!(stored_preview_count(&store_path), 0);
    }

    #[tokio::test]
    async fn final_authority_change_rejects_preparation_before_preview_persistence_or_staging() {
        let mut package = package_fixture::NativeFixture::new();
        let repository =
            package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
        let configured = configure_native_repository(&mut package, &repository).await;
        let workspace_root = configured.workspace_root().to_path_buf();
        let initial = authority_snapshot(&workspace_root, &repository);
        let mut changed = authority_snapshot(&workspace_root, &repository);
        changed.canonical_issue = CanonicalIssueSnapshot::new(
            ISSUE_ID,
            43,
            "Changed after workspace preparation",
            "The latest Issue body changed before preview registration.",
            vec!["The current criteria are different.".into()],
        )
        .unwrap();
        changed.profile.version += 1;
        changed.settings_revision += 1;
        let authority = TestAuthority::changing_on_final_read(initial, changed);
        let trusted_config = package.configured_trusted_config(&repository);
        let operation_log = trusted_config
            .state_root
            .join("native-preparation-fixture.log");
        let (preparer, worker_rpc, _server, _admission, store_path) =
            start_preparer(configured, authority.clone(), package.temp.path()).await;

        let error = preparer
            .prepare("main", native_request("main", None))
            .await
            .expect_err(
                "the final canonical snapshot must match the workspace's original snapshot",
            );

        assert!(matches!(error, NativePreparationError::AuthorityChanged));
        assert_eq!(authority.requests().len(), 2);
        assert_eq!(
            std::fs::read_to_string(operation_log)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            [
                "resolve_base main",
                "prepare main bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ],
            "the already-prepared workspace is retained without a second base resolution"
        );
        assert_eq!(stored_preview_count(&store_path), 0);
        assert_eq!(native_workers(&worker_rpc).len(), 1);
        assert_eq!(worker_rpc.counts().enqueue_for_stream_calls, 2);
        assert_eq!(worker_rpc.counts().enqueue_with_client_stream_calls, 0);
    }

    #[tokio::test]
    async fn shared_admission_lock_covers_final_read_and_preview_registration() {
        let mut package = package_fixture::NativeFixture::new();
        let repository =
            package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
        let configured = configure_native_repository(&mut package, &repository).await;
        let workspace_root = configured.workspace_root().to_path_buf();
        let snapshot = authority_snapshot(&workspace_root, &repository);
        let (authority, gate) = TestAuthority::pausing_on_read(snapshot.clone(), 1);
        let (preparer, worker_rpc, _server, admission, store_path) =
            start_preparer(configured, authority.clone(), package.temp.path()).await;
        assert!(Arc::ptr_eq(
            &admission,
            &preparer.manager().admission_lock()
        ));
        let prepare_task =
            tokio::spawn(
                async move { preparer.prepare("main", native_request("main", None)).await },
            );
        gate.entered.notified().await;

        let mutation_entered = Arc::new(Notify::new());
        let mutator = {
            let admission = Arc::clone(&admission);
            let authority = authority.clone();
            let mutation_entered = Arc::clone(&mutation_entered);
            tokio::spawn(async move {
                let _guard = admission.lock().await;
                let mut changed = authority.snapshot.lock().unwrap().clone();
                changed.settings_revision += 1;
                authority.replace_snapshot(changed);
                mutation_entered.notify_one();
            })
        };
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                mutation_entered.notified()
            )
            .await
            .is_err(),
            "the settings mutation must wait while final authority is compared and registered"
        );

        gate.release.notify_one();
        let preview = prepare_task.await.unwrap().unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            mutation_entered.notified(),
        )
        .await
        .unwrap();
        mutator.await.unwrap();
        assert_eq!(
            preview.expected_settings_revision(),
            snapshot.settings_revision
        );
        assert_eq!(stored_preview_count(&store_path), 1);
        assert_eq!(native_workers(&worker_rpc).len(), 1);
        assert_eq!(worker_rpc.counts().enqueue_for_stream_calls, 2);
        assert_eq!(worker_rpc.counts().enqueue_with_client_stream_calls, 0);
    }

    #[tokio::test]
    async fn persistent_mounts_cannot_overlap_sdk_native_control_package_or_missing_roots() {
        let mut package = package_fixture::NativeFixture::new();
        let repository =
            package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
        let configured = configure_native_repository(&mut package, &repository).await;
        let workspace_root = configured.workspace_root().to_path_buf();
        let base_snapshot = authority_snapshot(&workspace_root, &repository);
        let runtime_paths = host_worker_context(package.temp.path());
        let native_root = package.data_paths.root.join("coding-agent/native");
        let repository_partition = package_fixture::NativeFixture::partition(&repository);
        let configured_policy = package.configured_trusted_config(&repository);
        let operation_log = configured_policy
            .state_root
            .join("native-preparation-fixture.log");
        let sources = vec![
            runtime_paths.microsandbox_sdk_home.clone(),
            runtime_paths.microsandbox_sdk_home.join("cache"),
            runtime_paths
                .microsandbox_sdk_home
                .parent()
                .unwrap()
                .to_path_buf(),
            runtime_paths
                .backend_config_path
                .parent()
                .unwrap()
                .to_path_buf(),
            native_root.parent().unwrap().to_path_buf(),
            native_root.join("credential-sources"),
            native_root
                .join("repositories")
                .join(&repository_partition)
                .join("credentials"),
            configured_policy.publisher_root.clone(),
            native_root.join("configs").join(&repository_partition),
            native_root.join("repositories").join(&repository_partition),
            package.resource_root.clone(),
        ];
        let missing = package.temp.path().join("user-data/missing-mount");
        sources.iter().for_each(|source| {
            if !source.exists() {
                std::fs::create_dir_all(source).unwrap();
            }
        });
        let outside = package.temp.path().join("user-data/dangling-link");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.with_extension("missing-target"), &outside).unwrap();
        let symlink_target = package.temp.path().join("user-data/live-target");
        std::fs::create_dir_all(&symlink_target).unwrap();
        let live_symlink = package.temp.path().join("user-data/live-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&symlink_target, &live_symlink).unwrap();
        let mut all_sources = sources;
        all_sources.push(missing);
        #[cfg(unix)]
        all_sources.push(outside);
        #[cfg(unix)]
        all_sources.push(live_symlink);

        let authority = TestAuthority::new(base_snapshot.clone());
        let (preparer, worker_rpc, _server, _admission, store_path) =
            start_preparer(configured, authority.clone(), package.temp.path()).await;
        for source in all_sources {
            let snapshot = snapshot_with_persistent_mount(base_snapshot.clone(), &source, "/extra");
            authority.replace_snapshot(snapshot);
            let error = preparer
                .prepare("main", native_request("main", None))
                .await
                .expect_err(
                    "an explicit profile allow-root cannot override native host exclusions",
                );
            assert!(
                matches!(error, NativePreparationError::UnsafePersistentMount),
                "mount was unexpectedly admitted: {}",
                source.display()
            );
        }
        let target_overlap = package.temp.path().join("user-data/open-code-profile");
        std::fs::create_dir_all(&target_overlap).unwrap();
        let snapshot = snapshot_with_persistent_mount(
            base_snapshot,
            &target_overlap,
            "/workspace/session-shadow",
        );
        authority.replace_snapshot(snapshot);
        assert!(matches!(
            preparer.prepare("main", native_request("main", None)).await,
            Err(NativePreparationError::UnsafePersistentMount)
        ));
        assert!(worker_rpc.created_workers().is_empty());
        assert!(!operation_log.exists());
        assert_eq!(stored_preview_count(&store_path), 0);
    }

    #[tokio::test]
    async fn an_existing_user_approved_mount_outside_protected_roots_is_preserved_in_worker_settings()
     {
        let mut package = package_fixture::NativeFixture::new();
        let repository =
            package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
        let configured = configure_native_repository(&mut package, &repository).await;
        let workspace_root = configured.workspace_root().to_path_buf();
        let mount_root = package
            .temp
            .path()
            .join("native-runtime-fixture/sdk/microsandbox-profile");
        std::fs::create_dir_all(&mount_root).unwrap();
        let snapshot = snapshot_with_persistent_mount(
            authority_snapshot(&workspace_root, &repository),
            &mount_root,
            "/agent-profile",
        );
        let (preparer, worker_rpc, _server, _admission, _store_path) = start_preparer(
            configured,
            TestAuthority::new(snapshot),
            package.temp.path(),
        )
        .await;

        preparer
            .prepare("main", native_request("main", None))
            .await
            .expect("a dedicated approved profile-data mount remains usable");

        let workers = native_workers(&worker_rpc);
        assert_eq!(workers.len(), 1);
        let descriptor =
            JobworkerpProto::parse_runner_settings_schema_descriptor(&worker_rpc.runner_data())
                .unwrap()
                .unwrap();
        let settings = prost_reflect::DynamicMessage::decode(
            descriptor,
            workers[0].runner_settings.as_slice(),
        )
        .unwrap()
        .serialize(serde_json::value::Serializer)
        .unwrap();
        assert_eq!(
            settings["vm"]["mounts"][1]["hostPath"],
            mount_root.to_string_lossy().as_ref()
        );
        assert_eq!(settings["vm"]["mounts"][1]["guestPath"], "/agent-profile");
        assert!(
            !settings["vm"]["mounts"][1]["writable"]
                .as_bool()
                .unwrap_or_default()
        );
        assert!(
            !settings["vm"]["mounts"][1]["executable"]
                .as_bool()
                .unwrap_or_default()
        );
        assert_eq!(worker_rpc.counts().enqueue_for_stream_calls, 2);
        assert_eq!(worker_rpc.counts().enqueue_with_client_stream_calls, 0);
    }

    #[tokio::test]
    async fn configured_network_access_reaches_registered_worker_and_prepared_snapshot() {
        use lookback_tauri_lib::coding_agent::profile::NetworkAccess;
        for access in [
            NetworkAccess::Disabled,
            NetworkAccess::Public,
            NetworkAccess::PublicAndPrivate,
            NetworkAccess::PublicPrivateAndHost,
        ] {
            let mut package = package_fixture::NativeFixture::new();
            let repository =
                package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
            let configured = configure_native_repository(&mut package, &repository).await;
            let mut snapshot = authority_snapshot(configured.workspace_root(), &repository);
            snapshot.profile.network_access = access;
            let (preparer, rpc, _server, _admission, _) = start_preparer(
                configured,
                TestAuthority::new(snapshot),
                package.temp.path(),
            )
            .await;
            let prepared = preparer
                .prepare("main", native_request("main", None))
                .await
                .unwrap();
            let worker = native_workers(&rpc).pop().unwrap();
            let descriptor =
                JobworkerpProto::parse_runner_settings_schema_descriptor(&rpc.runner_data())
                    .unwrap()
                    .unwrap();
            let settings = prost_reflect::DynamicMessage::decode(
                descriptor,
                worker.runner_settings.as_slice(),
            )
            .unwrap()
            .serialize(serde_json::value::Serializer)
            .unwrap();
            assert_eq!(
                settings["network"]["enabled"],
                access != NetworkAccess::Disabled
            );
            let expected = access.runner_config();
            if access != NetworkAccess::Disabled {
                assert_eq!(settings["network"]["profiles"], expected["profiles"]);
                assert!(settings["network"]["maxTcpConnections"].as_u64().unwrap() > 0);
                assert!(settings["network"]["maxUdpConnections"].as_u64().unwrap() > 0);
            } else {
                assert!(settings["network"].get("profiles").is_none());
                assert!(settings["network"].get("maxTcpConnections").is_none());
            }
            assert_eq!(rpc.counts().enqueue_with_client_stream_calls, 0);
            let _ = prepared;
        }
    }

    #[tokio::test]
    async fn unsupported_profile_command_is_rejected_before_workspace_side_effects() {
        let mut package = package_fixture::NativeFixture::new();
        let repository =
            package_fixture::NativeFixture::repository(package_fixture::PRIMARY_REPOSITORY);
        preparation_rpc::install_preparation_executor(&mut package);
        let selection = package.selection().unwrap();
        let configured = lookback_tauri_lib::native_terminal::bootstrap::configure_repository(
            &package.data_paths,
            &selection,
            &repository,
        )
        .await
        .unwrap();
        let workspace_root = configured.workspace_root().to_path_buf();
        let mut snapshot = authority_snapshot(&workspace_root, &repository);
        snapshot.profile.command = vec!["opencode".into(), "exec".into()];
        let authority = TestAuthority::new(snapshot);
        let trusted_config = package.configured_trusted_config(&repository);
        let operation_log = trusted_config
            .state_root
            .join("native-preparation-fixture.log");
        let fixture_rpc = jobworkerp_rpc::RpcFixture::default();
        let server = jobworkerp_rpc::start_server(fixture_rpc.clone()).await;
        let preparer = NativeSessionPreparer::new(
            configured,
            Arc::new(authority),
            JobworkerpHandle::connect(&server.url).await.unwrap(),
            runtime(package.temp.path()),
            Arc::new(Mutex::new(())),
        );

        let error = preparer
            .prepare("main", native_request("main", None))
            .await
            .expect_err("only the approved interactive TUI command may be prepared");

        assert!(matches!(
            error,
            NativePreparationError::UnsupportedCliProfile
        ));
        assert!(!operation_log.exists());
        assert!(fixture_rpc.created_workers().is_empty());
    }

    #[test]
    fn preparation_runtime_accepts_a_missing_config_leaf_without_creating_it() {
        let directory = tempfile::tempdir_in("/tmp/opencode").unwrap();
        let mut context = host_worker_context(directory.path());
        std::fs::remove_file(&context.backend_config_path).unwrap();
        let config_path = context.backend_config_path.clone();
        assert!(NativePreparationRuntime::new(context.clone(), host_limits()).is_ok());
        assert!(!config_path.exists());

        let missing_parent = directory.path().join("missing-parent/config.json");
        context.backend_config_path = missing_parent;
        assert!(NativePreparationRuntime::new(context.clone(), host_limits()).is_err());
        context.backend_config_path = config_path.clone();
        std::os::unix::fs::symlink(directory.path().join("missing-target"), &config_path).unwrap();
        assert!(NativePreparationRuntime::new(context, host_limits()).is_err());
    }

    #[test]
    fn preparation_runtime_rejects_missing_or_untrusted_host_paths() {
        let error = NativePreparationRuntime::new(
            HostWorkerContext {
                microsandbox_sdk_home: Default::default(),
                backend_config_path: "/home/test/.local/share/lookback/msb/config.json".into(),
            },
            host_limits(),
        )
        .expect_err("host SDK context is required and cannot be renderer-selected");

        assert!(matches!(
            error,
            NativePreparationError::RuntimeContextUnavailable
        ));
    }

    #[test]
    fn profile_settings_worker_reports_a_missing_writable_home_separately() {
        use lookback_tauri_lib::jobworkerp::coding_agent::NativeWorkspaceBinding;
        use lookback_tauri_lib::native_terminal::preparation::{
            NativePreparationError, profile_settings_worker_spec,
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repository = repository(&RepositoryId {
            host: "forge.example".into(),
            owner: "team".into(),
            name: "project".into(),
        });
        let mut profile = profile(&root, &repository);
        let home = root.join("home");
        let work = root.join("settings-workspace");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&work).unwrap();
        let context = host_worker_context(&root);
        let workspace = NativeWorkspaceBinding {
            canonical_host_root: work,
            guest_root: "/workspace".into(),
        };

        // No home mount at all: the user has not prepared a dedicated home yet.
        assert_eq!(
            profile_settings_worker_spec(&profile, &workspace, &context, &[]).unwrap_err(),
            NativePreparationError::ProfileHomeUnavailable
        );

        // A read-only home cannot persist credentials either.
        profile.persistent_mounts = vec![PersistentMount {
            host_path: home.clone(),
            guest_path: profile.container_home.clone(),
            read_only: true,
            executable: false,
        }];
        assert_eq!(
            profile_settings_worker_spec(&profile, &workspace, &context, &[]).unwrap_err(),
            NativePreparationError::ProfileHomeUnavailable
        );

        // Other authority defects keep their existing classification.
        profile.persistent_mounts[0].read_only = false;
        let foreign_root = NativeWorkspaceBinding {
            guest_root: "/elsewhere".into(),
            ..workspace
        };
        assert_eq!(
            profile_settings_worker_spec(&profile, &foreign_root, &context, &[]).unwrap_err(),
            NativePreparationError::InvalidAuthoritySnapshot
        );
    }

    #[test]
    fn profile_settings_worker_shares_selected_home_without_issue_repository_or_source_mounts() {
        use lookback_tauri_lib::jobworkerp::coding_agent::NativeWorkspaceBinding;
        use lookback_tauri_lib::native_terminal::preparation::profile_settings_worker_spec;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repository = repository(&RepositoryId {
            host: "forge.example".into(),
            owner: "team".into(),
            name: "project".into(),
        });
        let mut profile = profile(&root, &repository);
        profile.allowed_repositories.clear();
        let home = root.join("home");
        let work = root.join("empty-settings-workspace");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&work).unwrap();
        profile.persistent_mounts = vec![PersistentMount {
            host_path: home.clone(),
            guest_path: profile.container_home.clone(),
            read_only: false,
            executable: true,
        }];
        let context = host_worker_context(&root);
        let workspace = NativeWorkspaceBinding {
            canonical_host_root: work,
            guest_root: "/workspace".into(),
        };
        let spec = profile_settings_worker_spec(&profile, &workspace, &context, &[]).unwrap();
        assert_eq!(spec.native_profile_id, PROFILE_ID);
        assert_eq!(
            spec.runner_settings_json["vm"]["env"]["HOME"],
            profile.container_home
        );
        let mount = &spec.runner_settings_json["vm"]["mounts"][1];
        assert_eq!(mount["host_path"], home.to_str().unwrap());
        assert_eq!(mount["writable"], true);
        assert_eq!(mount["executable"], true);
        profile.persistent_mounts[0].read_only = true;
        assert!(profile_settings_worker_spec(&profile, &workspace, &context, &[]).is_err());
        profile.persistent_mounts[0].read_only = false;
        assert!(
            profile_settings_worker_spec(
                &profile,
                &workspace,
                &context,
                std::slice::from_ref(&home)
            )
            .is_err()
        );
        std::fs::remove_dir(&home).unwrap();
        std::os::unix::fs::symlink(&context.microsandbox_sdk_home, &home).unwrap();
        assert!(profile_settings_worker_spec(&profile, &workspace, &context, &[]).is_err());
    }
}
