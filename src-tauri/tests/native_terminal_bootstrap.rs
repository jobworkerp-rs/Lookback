#[cfg(unix)]
mod unix_tests {
    use std::{
        collections::HashSet,
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
    };

    use coding_agent_executor::{EXECUTOR_VERSION, PROTOCOL_VERSION};
    use lookback_tauri_lib::coding_agent::settings::{
        CloneTransportSettings, CodingAgentSettings, CodingAgentSettingsStore,
        NativeRepositoryApprovalSettings, RepositoryHostSettings, RepositoryIdentitySettings,
    };
    use lookback_tauri_lib::native_terminal::bootstrap::{
        NativeBootstrapUnavailable, NativeExecutorRuntimeStatus, NativePackageSelection,
        NativeWorkspaceExecutorError, TRUSTED_HOST_POLICY_RELATIVE_PATH, configure_repository,
    };
    use serde_json::json;

    mod fixture {
        include!("fixtures/native_terminal_package.rs");
    }

    #[tokio::test]
    async fn configured_repository_is_private_restricted_and_not_runtime_ready() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture
            .selection()
            .expect("bundled package is structurally valid");
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);

        let configured = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .expect("approved interactive repository configures");

        assert_eq!(configured.repository(), &repository);
        assert_eq!(
            configured.runtime_status(),
            NativeExecutorRuntimeStatus::NotObserved,
            "the bundled helper's capability response is not SDK/runtime compatibility proof"
        );
        assert_eq!(configured.trusted_config_digest().len(), 64);
        assert_eq!(
            configured.workspace_root().file_name().unwrap(),
            "workspace"
        );
        assert!(
            configured
                .workspace_root()
                .starts_with(&fixture.data_paths.root)
        );

        let store_path = configured.session_store().database_path();
        let output_path = configured.output_store().database_path();
        assert_ne!(store_path, output_path);
        assert!(store_path.starts_with(&fixture.data_paths.root));
        assert!(output_path.starts_with(&fixture.data_paths.root));
        assert!(
            store_path
                .parent()
                .unwrap()
                .starts_with(&fixture.data_paths.root)
        );
        assert!(
            output_path
                .parent()
                .unwrap()
                .starts_with(&fixture.data_paths.root)
        );
        assert_private_file(store_path);
        assert!(
            !output_path.exists(),
            "terminal display does not create a transcript file"
        );

        let config = fixture::NativeFixture::configured_trusted_config(&fixture, &repository);
        assert_eq!(config.protocol_version, PROTOCOL_VERSION);
        assert_eq!(config.expected_executor_version, EXECUTOR_VERSION);
        assert_eq!(config.repositories.len(), 1);
        assert_eq!(config.repositories[0].id, repository);
        assert_eq!(
            config.repositories[0].read_credential_source.as_deref(),
            Some("read-token")
        );
        assert!(config.repositories[0].publish_credential_source.is_none());
        assert!(config.repositories[0].forge.is_none());
        assert_eq!(config.credential_sources.len(), 1);
        assert_ne!(
            fixture.source_token.as_deref(),
            Some(
                config
                    .credential_root
                    .join(&config.credential_sources[0].token_file)
                    .as_path()
            )
        );
        assert_eq!(
            fs::read(
                config
                    .credential_root
                    .join(&config.credential_sources[0].token_file)
            )
            .unwrap(),
            fixture::READ_TOKEN.as_bytes()
        );

        let debug = format!("{configured:?}");
        assert!(!debug.contains(fixture::READ_TOKEN));
        assert!(!debug.contains("github.com/lookback/agent-app.git"));
    }

    #[tokio::test]
    async fn stable_policy_and_read_credential_reuse_an_immutable_config_snapshot() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);

        let first = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap();
        let second = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap();

        assert_eq!(
            first.trusted_config_digest(),
            second.trusted_config_digest()
        );
        assert_eq!(first.workspace_root(), second.workspace_root());
        let config_dir = fixture
            .data_paths
            .root
            .join("coding-agent/native/configs")
            .join(fixture::NativeFixture::partition(&repository));
        assert_eq!(fs::read_dir(config_dir).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn repository_partitions_are_disjoint_and_never_share_trusted_roots() {
        let fixture = fixture::NativeFixture::new();
        fixture.configure_policy_repositories(&[
            fixture::PRIMARY_REPOSITORY,
            fixture::SECONDARY_REPOSITORY,
        ]);
        let selection = fixture.selection().unwrap();
        let first = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
        let second = fixture::NativeFixture::repository(fixture::SECONDARY_REPOSITORY);

        let first = configure_repository(&fixture.data_paths, &selection, &first)
            .await
            .unwrap();
        let second = configure_repository(&fixture.data_paths, &selection, &second)
            .await
            .unwrap();

        assert_ne!(first.workspace_root(), second.workspace_root());
        assert_ne!(
            first.session_store().database_path(),
            second.session_store().database_path()
        );
        assert_ne!(
            first.output_store().database_path(),
            second.output_store().database_path()
        );
        let first_config =
            fixture::NativeFixture::configured_trusted_config(&fixture, first.repository());
        let second_config =
            fixture::NativeFixture::configured_trusted_config(&fixture, second.repository());
        for first_root in [
            &first_config.workspace_root,
            &first_config.publisher_root,
            &first_config.state_root,
            &first_config.credential_root,
        ] {
            for second_root in [
                &second_config.workspace_root,
                &second_config.publisher_root,
                &second_config.state_root,
                &second_config.credential_root,
            ] {
                assert!(!first_root.starts_with(second_root));
                assert!(!second_root.starts_with(first_root));
            }
        }
        for config in [&first_config, &second_config] {
            let roots = [
                &config.workspace_root,
                &config.publisher_root,
                &config.state_root,
                &config.credential_root,
            ];
            for left in 0..roots.len() {
                for right in left + 1..roots.len() {
                    assert!(!roots[left].starts_with(roots[right]));
                    assert!(!roots[right].starts_with(roots[left]));
                }
            }
        }
    }

    #[tokio::test]
    async fn a_repository_not_in_host_policy_is_unavailable_without_config_creation() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let unknown = fixture::NativeFixture::repository(("github.com", "attacker", "repo"));

        let unavailable = configure_repository(&fixture.data_paths, &selection, &unknown)
            .await
            .unwrap_err();

        assert_eq!(
            unavailable,
            NativeBootstrapUnavailable::RepositoryNotApproved
        );
        assert!(
            !fixture
                .data_paths
                .root
                .join("coding-agent/native/configs")
                .exists()
        );
        assert!(!unavailable.user_message().contains("attacker"));
    }

    #[tokio::test]
    async fn missing_fixed_host_policy_starts_with_an_empty_gui_baseline() {
        let fixture = fixture::NativeFixture::new();
        fs::remove_file(fixture.policy_path()).unwrap();
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);

        let unavailable = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap_err();

        assert_eq!(
            unavailable,
            NativeBootstrapUnavailable::RepositoryNotApproved
        );
        assert!(!unavailable.user_message().is_empty());
        assert!(
            fixture
                .data_paths
                .root
                .join(TRUSTED_HOST_POLICY_RELATIVE_PATH)
                .parent()
                .unwrap()
                .is_dir()
        );
    }

    #[tokio::test]
    async fn missing_policy_accepts_saved_gui_ssh_repository_after_package_approval() {
        let fixture = fixture::NativeFixture::new();
        fs::remove_file(fixture.policy_path()).unwrap();
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
        let ssh_directory = fixture.temp.path().join("saved-ssh");
        fs::create_dir(&ssh_directory).unwrap();
        fs::set_permissions(&ssh_directory, fs::Permissions::from_mode(0o700)).unwrap();

        let settings_store =
            CodingAgentSettingsStore::new(fixture.data_paths.coding_agent_settings_path());
        let settings = CodingAgentSettings {
            repository_hosts: vec![RepositoryHostSettings {
                label_host: "github.com".into(),
                forge: lookback_tauri_lib::coding_agent::ForgeKind::GitHub,
                api_base_url: "https://api.github.com".into(),
                clone_transport: CloneTransportSettings::Ssh {
                    host: "github.com".into(),
                    user: "git".into(),
                    port: Some(22),
                    ssh_directory: Some(ssh_directory.clone()),
                },
                read_credential_ref: None,
                publication_credential_ref: None,
            }],
            native_repository_approvals: Some(vec![NativeRepositoryApprovalSettings {
                repository: RepositoryIdentitySettings {
                    host: "github.com".into(),
                    organization: "lookback".into(),
                    repository: "agent-app".into(),
                },
                allowed_base_branches: vec!["main".into()],
            }]),
            ..Default::default()
        };
        let saved = settings_store
            .compare_and_swap(0, settings, &HashSet::new())
            .unwrap();
        settings_store
            .compare_and_swap_native_baseline(
                saved.revision,
                fixture::hash_file(
                    &fixture
                        .resource_root
                        .join("coding-agent-artifact-manifest.json"),
                ),
            )
            .unwrap();

        let configured = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .expect("saved GUI approval configures without a policy file");
        assert_eq!(configured.trusted_config_digest().len(), 64);
        let trusted_config =
            fixture::NativeFixture::configured_trusted_config(&fixture, &repository);
        assert_eq!(
            trusted_config.repositories[0].ssh_directory.as_ref(),
            Some(&ssh_directory)
        );
    }

    #[tokio::test]
    async fn a_changed_binary_after_package_selection_is_rejected_before_probe() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let helper = fixture.resource_root.join("bin/coding-agent-executor");
        fs::write(&helper, b"changed after the package was selected").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);

        let unavailable = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap_err();

        assert_eq!(unavailable, NativeBootstrapUnavailable::PackageUnavailable);
    }

    #[test]
    fn package_manifest_rejects_unknown_fields_and_artifact_hash_mismatch() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let mut unknown = fixture.manifest.clone();
        unknown["sources"]["agent_app"]["unchecked"] = json!(true);
        fixture::set_manifest(&fixture, &unknown);
        assert_eq!(
            NativePackageSelection::from_app_resources(&fixture.app_resource_dir).unwrap_err(),
            NativeBootstrapUnavailable::PackageUnavailable
        );

        fixture::set_manifest(&fixture, &fixture.manifest);
        let mut mismatched = fixture.manifest.clone();
        mismatched["artifacts"]["executor"]["sha256"] = json!("0".repeat(64));
        fixture::set_manifest(&fixture, &mismatched);
        assert_eq!(
            NativePackageSelection::from_app_resources(&fixture.app_resource_dir).unwrap_err(),
            NativeBootstrapUnavailable::PackageUnavailable
        );
        drop(selection);
    }

    #[test]
    fn package_artifact_symlinks_are_never_hashed_or_accepted() {
        let fixture = fixture::NativeFixture::new();
        let runtime = fixture.resource_root.join("runtime/all-in-one");
        let replacement = fixture.temp.path().join("outside-runtime");
        fs::write(&replacement, b"untrusted runtime target").unwrap();
        fs::remove_file(&runtime).unwrap();
        std::os::unix::fs::symlink(&replacement, &runtime).unwrap();

        assert_eq!(
            NativePackageSelection::from_app_resources(&fixture.app_resource_dir).unwrap_err(),
            NativeBootstrapUnavailable::PackageUnavailable
        );
    }

    #[test]
    fn writable_package_subdirectories_are_rejected_before_hashing() {
        let fixture = fixture::NativeFixture::new();
        fs::set_permissions(
            fixture.resource_root.join("bin"),
            fs::Permissions::from_mode(0o777),
        )
        .unwrap();

        assert_eq!(
            NativePackageSelection::from_app_resources(&fixture.app_resource_dir).unwrap_err(),
            NativeBootstrapUnavailable::PackageUnavailable
        );
    }

    #[tokio::test]
    async fn malformed_policy_is_fixed_code_and_does_not_echo_paths_or_secrets() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let policy_path = fixture.policy_path();
        let mut policy: serde_json::Value =
            serde_json::from_slice(&fs::read(&policy_path).unwrap()).unwrap();
        policy["renderer_executable"] = json!("/secret/renderer-controlled/path");
        fixture::write_private_file(
            &policy_path,
            serde_json::to_string(&policy).unwrap().as_bytes(),
        );
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);

        let unavailable = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap_err();

        assert_eq!(
            unavailable,
            NativeBootstrapUnavailable::InvalidTrustedPolicy
        );
        assert!(!unavailable.user_message().contains("renderer-controlled"));
        assert!(!format!("{unavailable:?}").contains("secret"));
    }

    #[tokio::test]
    async fn duplicate_policy_fields_and_mismatched_clone_identity_fail_closed() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
        let policy_path = fixture.policy_path();
        fixture::write_private_file(&policy_path, br#"{"version":1,"version":1}"#);
        assert_eq!(
            configure_repository(&fixture.data_paths, &selection, &repository)
                .await
                .unwrap_err(),
            NativeBootstrapUnavailable::InvalidTrustedPolicy
        );

        fixture::write_host_policy(
            &fixture.data_paths,
            &fixture.manifest,
            &[fixture::PRIMARY_REPOSITORY],
            None,
        );
        let mut policy: serde_json::Value =
            serde_json::from_slice(&fs::read(&policy_path).unwrap()).unwrap();
        policy["repositories"][0]["clone_url"] =
            json!("https://token:secret@evil.example/other/repo.git");
        fixture::write_private_file(
            &policy_path,
            serde_json::to_string(&policy).unwrap().as_bytes(),
        );
        let unavailable = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap_err();
        assert_eq!(
            unavailable,
            NativeBootstrapUnavailable::InvalidTrustedPolicy
        );
        assert!(!unavailable.user_message().contains("evil.example"));
        assert!(!format!("{unavailable:?}").contains("secret"));
    }

    #[tokio::test]
    async fn a_symlinked_fixed_policy_is_rejected_without_following_it() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let policy_path = fixture.policy_path();
        let replacement = fixture.temp.path().join("replacement-policy.json");
        fs::copy(&policy_path, &replacement).unwrap();
        fs::remove_file(&policy_path).unwrap();
        std::os::unix::fs::symlink(&replacement, &policy_path).unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);

        let unavailable = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap_err();

        assert_eq!(unavailable, NativeBootstrapUnavailable::UnsafeTrustedPolicy);
    }

    #[tokio::test]
    async fn package_identity_pin_rejects_a_different_valid_source_manifest() {
        let fixture = fixture::NativeFixture::new();
        let arbitrary_resource_dir = fixture.temp.path().join("other-resources");
        let arbitrary_package = arbitrary_resource_dir.join("coding-agent");
        fs::create_dir_all(&arbitrary_package).unwrap();
        fs::set_permissions(&arbitrary_package, fs::Permissions::from_mode(0o700)).unwrap();
        for relative in [
            "bin/coding-agent-executor",
            "runtime/all-in-one",
            "workers/lookback-coding-agent-workers.yaml",
            "workers/workflows/coding-agent/prepare.yaml",
            "workers/workflows/coding-agent/one-shot.yaml",
            "workers/workflows/coding-agent/publish.yaml",
        ] {
            let destination = arbitrary_package.join(relative);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::set_permissions(
                destination.parent().unwrap(),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            fs::copy(fixture.resource_root.join(relative), &destination).unwrap();
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut other_manifest = fixture.manifest.clone();
        other_manifest["sources"]["agent_app"]["revision"] = json!("e".repeat(40));
        fixture::write_private_file(
            &arbitrary_package.join("coding-agent-artifact-manifest.json"),
            serde_json::to_string_pretty(&other_manifest)
                .unwrap()
                .as_bytes(),
        );
        let selected = NativePackageSelection::from_app_resources(&arbitrary_resource_dir).unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
        let expected = fixture.manifest["executor"]["version"].as_str().unwrap();
        assert_eq!(expected, EXECUTOR_VERSION);
        assert_eq!(
            fixture.manifest["executor"]["protocol_version"].as_u64(),
            Some(u64::from(PROTOCOL_VERSION))
        );
        assert_eq!(selected.package_version(), EXECUTOR_VERSION);
        assert_eq!(repository.host, "github.com");
        let unavailable = configure_repository(&fixture.data_paths, &selected, &repository)
            .await
            .unwrap_err();
        assert_eq!(
            unavailable,
            NativeBootstrapUnavailable::PackageIdentityMismatch
        );
        assert!(
            !fixture
                .data_paths
                .root
                .join("coding-agent/native/configs")
                .exists()
        );
    }

    #[tokio::test]
    async fn read_base_capability_is_optional_without_disabling_the_package() {
        let fixture = fixture::NativeFixture::new();
        let mut manifest = fixture.manifest.clone();
        let features = manifest["executor"]["capabilities"].as_array_mut().unwrap();
        features.retain(|feature| feature != "resolve_base");
        let helper_features = features
            .iter()
            .map(|feature| feature.as_str().unwrap())
            .collect::<Vec<_>>();
        fixture.set_capabilities(&helper_features);
        manifest = fixture::manifest_json_with_features(&fixture.resource_root, &helper_features);
        fixture::set_manifest(&fixture, &manifest);
        fixture::write_host_policy(
            &fixture.data_paths,
            &manifest,
            &[fixture::PRIMARY_REPOSITORY],
            None,
        );
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);

        let configured = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .expect("the required protocol is still compatible without optional resolve_base");

        assert_eq!(
            configured.runtime_status(),
            NativeExecutorRuntimeStatus::NotObserved
        );
        assert!(!configured.executor().supports_resolve_base());
        let unavailable = configured
            .executor()
            .resolve_base(coding_agent_executor::ResolveBaseRequest {
                run_id: "run-native-01".into(),
                attempt_id: "attempt-native-01".into(),
                operation_id: "operation-native-01".into(),
                repository,
                base_branch: "main".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(
            unavailable,
            NativeWorkspaceExecutorError::ResolveBaseUnavailable
        );
    }

    #[tokio::test]
    async fn prepare_only_package_configures_without_artifact_or_publication_capabilities() {
        let fixture = fixture::NativeFixture::new();
        let core_features = ["prepare"];
        fixture.set_capabilities(&core_features);
        let manifest = fixture::manifest_json_with_features(&fixture.resource_root, &core_features);
        fixture::set_manifest(&fixture, &manifest);
        fixture::write_host_policy(
            &fixture.data_paths,
            &manifest,
            &[fixture::PRIMARY_REPOSITORY],
            None,
        );
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);

        let configured = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .expect("protocol core operations are enough for workspace bootstrap");

        assert_eq!(
            configured.runtime_status(),
            NativeExecutorRuntimeStatus::NotObserved
        );
        assert!(!configured.executor().supports_resolve_base());
        assert_eq!(
            configured
                .executor()
                .resolve_base(coding_agent_executor::ResolveBaseRequest {
                    run_id: "run-native-01".into(),
                    attempt_id: "attempt-native-01".into(),
                    operation_id: "operation-native-01".into(),
                    repository,
                    base_branch: "main".into(),
                })
                .await
                .unwrap_err(),
            NativeWorkspaceExecutorError::ResolveBaseUnavailable
        );
    }

    #[tokio::test]
    async fn probed_capabilities_must_match_the_pinned_manifest_exactly() {
        let fixture = fixture::NativeFixture::new();
        let no_base_resolution = fixture::FEATURES
            .iter()
            .copied()
            .filter(|feature| *feature != "resolve_base")
            .collect::<Vec<_>>();
        let manifest =
            fixture::manifest_json_with_features(&fixture.resource_root, &no_base_resolution);
        fixture::set_manifest(&fixture, &manifest);
        fixture::write_host_policy(
            &fixture.data_paths,
            &manifest,
            &[fixture::PRIMARY_REPOSITORY],
            None,
        );
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);

        let unavailable = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap_err();

        assert_eq!(unavailable, NativeBootstrapUnavailable::ExecutorUnavailable);
    }

    #[tokio::test]
    async fn optional_workspace_publication_capability_survives_package_selection_and_probe() {
        let fixture = fixture::NativeFixture::new();
        let features = [fixture::FEATURES, &["push_workspace"]].concat();
        fixture.set_capabilities(&features);
        let manifest = fixture::manifest_json_with_features(&fixture.resource_root, &features);
        fixture::set_manifest(&fixture, &manifest);
        fixture::write_host_policy(
            &fixture.data_paths,
            &manifest,
            &[fixture::PRIMARY_REPOSITORY],
            None,
        );
        let selection = fixture
            .selection()
            .expect("new optional capability is a supported package");
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
        let configured = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .expect("actual helper handshake matches the package capability");
        assert_eq!(
            configured.runtime_status(),
            NativeExecutorRuntimeStatus::NotObserved
        );
    }

    #[tokio::test]
    async fn explicit_publication_credentials_and_forge_are_bound_to_generated_configuration() {
        for mode in ["independent", "shared", "publish_only"] {
            let fixture = fixture::NativeFixture::new();
            let selection = fixture.selection().unwrap();
            let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
            let path = fixture.policy_path();
            let mut policy: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let source = path
                .parent()
                .unwrap()
                .join("credential-sources/publish-token");
            fixture::write_private_file(&source, b"fixture-publish-credential-not-for-logging\n");
            let id = if mode == "shared" {
                "read-token"
            } else {
                "publish-token"
            };
            if mode != "shared" {
                policy["credential_sources"].as_array_mut().unwrap().push(
                    json!({"id":"publish-token","username":"fixture","token_file":"publish-token"}),
                );
            }
            if mode == "publish_only" {
                policy["repositories"][0]["read_credential_source"] = serde_json::Value::Null;
            }
            policy["repositories"][0]["publish_credential_source"] = json!(id);
            let forge = json!({"kind":"github","api_base_url":"https://api.github.com","web_base_url":"https://github.com"});
            policy["repositories"][0]["forge"] = forge.clone();
            fixture::write_private_file(&path, &serde_json::to_vec(&policy).unwrap());
            configure_repository(&fixture.data_paths, &selection, &repository)
                .await
                .expect("explicit publication authority configures");
            let generated = fixture.configured_trusted_config(&repository);
            assert_eq!(
                generated.repositories[0]
                    .publish_credential_source
                    .as_deref(),
                Some(id)
            );
            assert_eq!(
                serde_json::to_value(&generated.repositories[0].forge).unwrap(),
                forge
            );
            assert_eq!(
                generated.credential_sources.len(),
                if mode == "independent" { 2 } else { 1 }
            );
            let copied = generated
                .credential_sources
                .iter()
                .find(|source| source.id == id)
                .unwrap();
            let original = if mode == "shared" {
                fixture.source_token.as_ref().unwrap()
            } else {
                &source
            };
            assert_ne!(
                generated.credential_root.join(&copied.token_file),
                *original
            );
            let original_bytes = fs::read(original).unwrap();
            assert_eq!(
                fs::read(generated.credential_root.join(&copied.token_file)).unwrap(),
                original_bytes
                    .strip_suffix(b"\n")
                    .unwrap_or(&original_bytes)
            );
        }
    }

    #[tokio::test]
    async fn publication_credentials_reject_missing_references_and_unsafe_secret_files() {
        use std::os::unix::fs::symlink;
        for failure in ["reference", "missing", "symlink"] {
            let fixture = fixture::NativeFixture::new();
            let selection = fixture.selection().unwrap();
            let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
            let path = fixture.policy_path();
            let mut policy: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            policy["repositories"][0]["publish_credential_source"] = json!("publish-token");
            if failure != "reference" {
                policy["credential_sources"].as_array_mut().unwrap().push(
                    json!({"id":"publish-token","username":"fixture","token_file":"publish-token"}),
                );
            }
            if failure == "symlink" {
                symlink(
                    fixture.source_token.as_ref().unwrap(),
                    path.parent()
                        .unwrap()
                        .join("credential-sources/publish-token"),
                )
                .unwrap();
            }
            fixture::write_private_file(&path, &serde_json::to_vec(&policy).unwrap());
            let rejected = configure_repository(&fixture.data_paths, &selection, &repository)
                .await
                .unwrap_err();
            assert_eq!(
                rejected,
                if failure == "reference" {
                    NativeBootstrapUnavailable::InvalidTrustedPolicy
                } else {
                    NativeBootstrapUnavailable::CredentialUnavailable
                }
            );
        }
    }

    #[tokio::test]
    async fn unselected_repository_credentials_are_not_read_or_snapshotted() {
        use std::os::unix::fs::symlink;
        for unsafe_file in [false, true] {
            let fixture = fixture::NativeFixture::new();
            let selection = fixture.selection().unwrap();
            let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
            let path = fixture.policy_path();
            let mut policy: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            policy["repositories"][0]["publish_credential_source"] = json!("read-token");
            let mut other = policy["repositories"][0].clone();
            other["id"] = serde_json::to_value(fixture::NativeFixture::repository(
                fixture::SECONDARY_REPOSITORY,
            ))
            .unwrap();
            other["clone_url"] = json!("https://github.com/lookback/second-app.git");
            other["read_credential_source"] = json!("unused-token");
            other["publish_credential_source"] = json!("unused-token");
            policy["repositories"][1] = other;
            policy["credential_sources"].as_array_mut().unwrap().push(
                json!({"id":"unused-token","username":"fixture","token_file":"unused-token"}),
            );
            if unsafe_file {
                symlink(
                    fixture.source_token.as_ref().unwrap(),
                    path.parent()
                        .unwrap()
                        .join("credential-sources/unused-token"),
                )
                .unwrap();
            }
            fixture::write_private_file(&path, &serde_json::to_vec(&policy).unwrap());
            configure_repository(&fixture.data_paths, &selection, &repository)
                .await
                .expect("unselected credential files are never opened");
            let generated = fixture.configured_trusted_config(&repository);
            assert_eq!(generated.repositories.len(), 1);
            assert_eq!(generated.credential_sources.len(), 1);
            assert_eq!(generated.credential_sources[0].id, "read-token");
            assert_eq!(fs::read_dir(&generated.credential_root).unwrap().count(), 1);
        }
    }

    #[tokio::test]
    async fn trusted_policy_parent_and_file_must_keep_the_fixed_private_modes() {
        let fixture = fixture::NativeFixture::new();
        let policy_path = fixture.policy_path();
        fs::set_permissions(
            policy_path.parent().unwrap(),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
        let outcome = configure_repository(&fixture.data_paths, &selection, &repository).await;
        assert_eq!(
            outcome.unwrap_err(),
            NativeBootstrapUnavailable::UnsafeTrustedPolicy
        );

        fs::set_permissions(
            policy_path.parent().unwrap(),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::set_permissions(&policy_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            configure_repository(&fixture.data_paths, &selection, &repository)
                .await
                .unwrap_err(),
            NativeBootstrapUnavailable::UnsafeTrustedPolicy
        );
    }

    #[tokio::test]
    async fn helper_configuration_is_immutable_private_and_uses_only_repo_scoped_roots() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
        let configured = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap();
        let config = fixture::NativeFixture::configured_trusted_config(&fixture, &repository);

        for root in [
            &config.workspace_root,
            &config.publisher_root,
            &config.state_root,
            &config.credential_root,
        ] {
            assert!(root.is_absolute());
            assert_eq!(fs::canonicalize(root).unwrap(), *root);
            assert_eq!(fs::metadata(root).unwrap().mode() & 0o777, 0o700);
        }
        let file = fs::read_dir(
            fixture
                .data_paths
                .root
                .join("coding-agent/native/configs")
                .join(fixture::NativeFixture::partition(&repository)),
        )
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
        assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(configured.trusted_config_digest().len(), 64);
    }

    #[tokio::test]
    async fn unsafe_app_data_ancestor_is_rejected_before_native_roots_are_created() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
        let data_parent = fixture.data_paths.root.parent().unwrap();
        let previous_mode = fs::metadata(data_parent).unwrap().mode() & 0o7777;
        fs::set_permissions(data_parent, fs::Permissions::from_mode(0o777)).unwrap();

        let outcome = configure_repository(&fixture.data_paths, &selection, &repository).await;

        fs::set_permissions(data_parent, fs::Permissions::from_mode(previous_mode)).unwrap();
        assert_eq!(
            outcome.unwrap_err(),
            NativeBootstrapUnavailable::UnsafeTrustedPolicy
        );
        let native_root = fixture.data_paths.root.join("coding-agent/native");
        assert!(!native_root.join("repositories").exists());
        assert!(!native_root.join("configs").exists());
    }

    #[tokio::test]
    async fn unsafe_package_ancestor_is_rejected_before_import_roots_are_created() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
        let resource_parent = fixture.app_resource_dir.as_path();
        let previous_mode = fs::metadata(resource_parent).unwrap().mode() & 0o7777;
        fs::set_permissions(resource_parent, fs::Permissions::from_mode(0o777)).unwrap();

        let outcome = configure_repository(&fixture.data_paths, &selection, &repository).await;

        fs::set_permissions(resource_parent, fs::Permissions::from_mode(previous_mode)).unwrap();
        assert_eq!(
            outcome.unwrap_err(),
            NativeBootstrapUnavailable::PackageUnavailable
        );
        let native_root = fixture.data_paths.root.join("coding-agent/native");
        assert!(!native_root.join("repositories").exists());
        assert!(!native_root.join("configs").exists());
    }

    #[tokio::test]
    async fn credential_rotation_keeps_the_old_private_snapshot_unchanged() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
        let first = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap();
        let first_config_path = fixture
            .data_paths
            .root
            .join("coding-agent/native/configs")
            .join(fixture::NativeFixture::partition(&repository))
            .join(format!("trusted-{}.json", first.trusted_config_digest()));
        let first_config_bytes = fs::read(&first_config_path).unwrap();
        let first_config: coding_agent_executor::TrustedConfig =
            serde_json::from_slice(&first_config_bytes).unwrap();
        let first_token_path = first_config
            .credential_root
            .join(&first_config.credential_sources[0].token_file);
        assert_eq!(
            fs::read(&first_token_path).unwrap(),
            fixture::READ_TOKEN.as_bytes()
        );

        let source_token = fixture.source_token.as_ref().unwrap();
        fixture::write_private_file(source_token, b"rotated-read-token");
        let second = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap();
        let second_config_path = fixture
            .data_paths
            .root
            .join("coding-agent/native/configs")
            .join(fixture::NativeFixture::partition(&repository))
            .join(format!("trusted-{}.json", second.trusted_config_digest()));
        let second_config: coding_agent_executor::TrustedConfig =
            serde_json::from_slice(&fs::read(&second_config_path).unwrap()).unwrap();
        let second_token_path = second_config
            .credential_root
            .join(&second_config.credential_sources[0].token_file);

        assert_ne!(
            first.trusted_config_digest(),
            second.trusted_config_digest()
        );
        assert_eq!(
            fs::read(&first_token_path).unwrap(),
            fixture::READ_TOKEN.as_bytes()
        );
        assert_eq!(fs::read(&second_token_path).unwrap(), b"rotated-read-token");
        assert_eq!(fs::read(&first_config_path).unwrap(), first_config_bytes);
    }

    #[tokio::test]
    async fn a_corrupt_credential_snapshot_is_never_overwritten_or_removed() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
        let configured = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap();
        let config_dir = fixture
            .data_paths
            .root
            .join("coding-agent/native/configs")
            .join(fixture::NativeFixture::partition(&repository));
        let config_path = config_dir.join(format!(
            "trusted-{}.json",
            configured.trusted_config_digest()
        ));
        let config_before = fs::read(&config_path).unwrap();
        let trusted: coding_agent_executor::TrustedConfig =
            serde_json::from_slice(&config_before).unwrap();
        let credential_root = trusted.credential_root;
        let credential_path = credential_root.join(&trusted.credential_sources[0].token_file);
        fs::write(&credential_path, b"partial-or-corrupt").unwrap();
        fs::set_permissions(&credential_path, fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(
            configure_repository(&fixture.data_paths, &selection, &repository)
                .await
                .unwrap_err(),
            NativeBootstrapUnavailable::CredentialUnavailable
        );
        assert_eq!(fs::read(&credential_path).unwrap(), b"partial-or-corrupt");
        assert_eq!(fs::read(&config_path).unwrap(), config_before);
        assert_eq!(fs::read_dir(credential_root).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn credential_reference_cannot_escape_the_fixed_host_credential_root() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let policy_path = fixture.policy_path();
        let mut policy: serde_json::Value =
            serde_json::from_slice(&fs::read(&policy_path).unwrap()).unwrap();
        policy["credential_sources"][0]["token_file"] = json!("../../outside-secret");
        fixture::write_private_file(
            &policy_path,
            serde_json::to_string(&policy).unwrap().as_bytes(),
        );
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);

        let unavailable = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap_err();

        assert_eq!(
            unavailable,
            NativeBootstrapUnavailable::InvalidTrustedPolicy
        );
        assert!(!unavailable.user_message().contains("outside-secret"));
    }

    #[tokio::test]
    #[ignore = "requires a locally built, explicitly selected coding-agent-executor binary"]
    async fn locally_built_executor_uses_the_same_native_configuration_and_probe_path() {
        let binary = std::env::var_os("LOOKBACK_CODING_AGENT_EXECUTOR_BIN")
            .expect("set LOOKBACK_CODING_AGENT_EXECUTOR_BIN to the compiled executor");
        let fixture = fixture::NativeFixture::new();
        let bundled_binary = fixture.resource_root.join("bin/coding-agent-executor");
        fs::copy(binary, &bundled_binary).unwrap();
        fs::set_permissions(&bundled_binary, fs::Permissions::from_mode(0o700)).unwrap();
        let manifest = fixture::manifest_json(&fixture.resource_root);
        fixture::set_manifest(&fixture, &manifest);
        fixture::write_host_policy(
            &fixture.data_paths,
            &manifest,
            &[fixture::PRIMARY_REPOSITORY],
            None,
        );
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);

        let configured = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .expect("real --capabilities probe follows production package validation");

        assert_eq!(
            configured.runtime_status(),
            NativeExecutorRuntimeStatus::NotObserved
        );
    }

    fn assert_private_file(path: &std::path::Path) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    }
    #[tokio::test]
    async fn explicitly_selected_ssh_directory_is_bound_to_generated_repository_config() {
        let fixture = fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        let repository = fixture::NativeFixture::repository(fixture::PRIMARY_REPOSITORY);
        let ssh = fixture.data_paths.root.join("selected-ssh");
        fs::create_dir(&ssh).unwrap();
        let policy_path = fixture.policy_path();
        let mut policy: serde_json::Value =
            serde_json::from_slice(&fs::read(&policy_path).unwrap()).unwrap();
        policy["repositories"][0]["clone_url"] = json!(format!(
            "ssh://deploy@ssh.code.example:22/{}/{}.git",
            repository.owner, repository.name
        ));
        policy["repositories"][0]["read_credential_source"] = serde_json::Value::Null;
        policy["repositories"][0]["ssh_directory"] = serde_json::to_value(&ssh).unwrap();
        fixture::write_private_file(
            &policy_path,
            serde_json::to_vec(&policy).unwrap().as_slice(),
        );
        let configured = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .expect("explicit host SSH directory configures");
        let generated = fixture.configured_trusted_config(&repository);
        let generated = serde_json::to_value(generated).unwrap();
        assert_eq!(
            generated["repositories"][0]["ssh_directory"],
            serde_json::to_value(&ssh).unwrap()
        );
        assert!(
            generated["credential_sources"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let first_digest = configured.trusted_config_digest().to_owned();
        let second = fixture.data_paths.root.join("different-ssh");
        fs::create_dir(&second).unwrap();
        policy["repositories"][0]["ssh_directory"] = serde_json::to_value(second).unwrap();
        fixture::write_private_file(
            &policy_path,
            serde_json::to_vec(&policy).unwrap().as_slice(),
        );
        let changed = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap();
        assert_ne!(first_digest, changed.trusted_config_digest());
    }
}
