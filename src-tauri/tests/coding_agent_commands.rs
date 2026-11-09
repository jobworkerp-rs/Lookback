use std::{
    collections::{BTreeMap, HashSet},
    fs,
};

use lookback_tauri_lib::coding_agent::settings::{
    AgentProfileSettings, CodingAgentSettings, CodingAgentSettingsStore,
    HostAgentConfigurationSettings, PersistentMountSettings, SandboxResourcesSettings,
    SettingsError,
};
use lookback_tauri_lib::coding_agent::{
    AgentProfileSnapshot, AgentRunSnapshot, AllowedOperation, ExecutionIdentity, ExecutionMode,
    ExecutionResult, ForgeKind, HostResourceLimits, InputDelivery, PublicationPlan,
    RepositoryIdentity, RepositoryProvenance, RepositorySelection, ResolvedRepository, RunApproval,
    RunId, RunPurpose, RunState, RunStore, SandboxResources, TerminalControl,
    TerminalOwnerRegistry, WorkflowDefinitionSnapshot,
};
use lookback_tauri_lib::commands::coding_agent::CodingAgentRunOwnerRegistry;
use lookback_tauri_lib::commands::{
    CodingAgentShutdownError, CodingAgentShutdownOperations, CodingAgentShutdownRun,
    reconcile_coding_agent_shutdown_runs,
};
use lookback_tauri_lib::data::DataPaths;
use lookback_tauri_lib::window_lifecycle::{
    BackgroundUnavailableReason, CloseActionResult, CloseChoice, ExitRetryPlan,
    NativeFallbackLocale, WindowLifecycle, WindowRestoreAdapter, plan_exit_retry,
};
use lookback_tauri_lib::{
    TrayAvailability, platform_tray_host_availability, tray_availability_for_icon,
};
use std::sync::atomic::{AtomicBool, Ordering};

const IMAGE: &str = "registry.example/agents/opencode:2026@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn empty_settings() -> CodingAgentSettings {
    CodingAgentSettings::default()
}

fn profile(profile_id: &str) -> AgentProfileSettings {
    AgentProfileSettings {
        profile_id: profile_id.into(),
        version: 1,
        display_name: "OpenCode".into(),
        image_ref: IMAGE.into(),
        allowed_image_refs: vec![IMAGE.into()],
        command: vec!["opencode".into(), "run".into()],
        container_home: "/home/agent".into(),
        host_configuration: None,
        runtime_environment: Default::default(),
        network_access: Default::default(),
        resources: SandboxResourcesSettings {
            cpu_millis: 1000,
            memory_mib: 2048,
            root_disk_mib: 8192,
            max_parallel_runs: 1,
            use_static: false,
        },
        mount_allowed_roots: Vec::new(),
        persistent_mounts: Vec::<PersistentMountSettings>::new(),
        allowed_repositories: Vec::new(),
        credential_reference: None,
    }
}

#[cfg(unix)]
#[test]
fn host_agent_configuration_derives_explicit_auth_mounts_without_mounting_container_home() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let profile_homes = root.join("profile-homes");
    let managed_home = profile_homes.join(
        lookback_tauri_lib::coding_agent::assessment::sha256_hex(b"opencode"),
    );
    fs::create_dir(&profile_homes).unwrap();
    fs::create_dir(&managed_home).unwrap();
    let shared_home = root.join("profile-shared-homes").join(
        lookback_tauri_lib::coding_agent::assessment::sha256_hex(b"opencode"),
    );
    fs::create_dir_all(&shared_home).unwrap();
    let codex = root.join("codex");
    let claude = root.join("claude");
    let claude_settings = root.join("claude.json");
    fs::create_dir(&codex).unwrap();
    fs::create_dir(&claude).unwrap();
    fs::write(&claude_settings, b"not inspected").unwrap();
    for path in [&profile_homes, &managed_home, &codex, &claude] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::set_permissions(&claude_settings, fs::Permissions::from_mode(0o600)).unwrap();

    let mut configured = profile("opencode");
    configured.mount_allowed_roots = vec![root.to_path_buf()];
    configured.persistent_mounts = vec![PersistentMountSettings {
        host_path: managed_home,
        guest_path: configured.container_home.clone(),
        read_only: false,
        executable: false,
    }];
    configured.host_configuration = Some(HostAgentConfigurationSettings {
        codex_directory: Some(codex.clone()),
        claude_directory: Some(claude.clone()),
        claude_settings_file: Some(claude_settings.clone()),
    });

    let effective = configured
        .to_agent_profile_with_managed_home_root(&profile_homes)
        .unwrap();
    let snapshot = effective
        .validate_and_snapshot(&HostResourceLimits {
            cpu_millis: 8000,
            memory_mib: 8192,
            root_disk_mib: 16384,
            max_parallel_runs: 8,
        })
        .unwrap();
    assert!(
        !snapshot
            .persistent_mounts
            .iter()
            .any(|mount| mount.guest_path == "/home/agent")
    );
    let overlay = snapshot.managed_home_overlay.as_ref().unwrap();
    assert!(
        overlay
            .children
            .iter()
            .any(|child| child.is_codex_directory(&fs::canonicalize(&codex).unwrap()))
    );
    assert!(
        overlay
            .children
            .iter()
            .any(|child| child.is_claude_directory(&fs::canonicalize(&claude).unwrap()))
    );
    assert!(
        !snapshot
            .persistent_mounts
            .iter()
            .any(|mount| mount.guest_path == "/home/agent/.claude.json")
    );
    assert_eq!(
        snapshot.runtime_environment.get("CODEX_HOME"),
        Some(&"/home/agent/.codex".to_owned())
    );
}

#[test]
fn host_agent_configuration_null_keeps_private_home_mount_compatible() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("profile-homes");
    let managed_home = home.join(lookback_tauri_lib::coding_agent::assessment::sha256_hex(
        b"opencode",
    ));
    fs::create_dir(&home).unwrap();
    fs::create_dir(&managed_home).unwrap();
    let mut configured = profile("opencode");
    configured.mount_allowed_roots = vec![temp.path().to_path_buf()];
    configured.persistent_mounts = vec![PersistentMountSettings {
        host_path: managed_home.clone(),
        guest_path: configured.container_home.clone(),
        read_only: false,
        executable: false,
    }];

    let effective = configured.to_agent_profile().unwrap();
    assert_eq!(effective.persistent_mounts[0].host_path, managed_home);
    assert_eq!(effective.persistent_mounts[0].guest_path, "/home/agent");
}

#[cfg(unix)]
#[test]
fn host_shared_mode_does_not_treat_an_unrelated_profile_home_shape_as_managed() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let impostor_root = temp.path().join("unrelated").join("profile-homes");
    let impostor_home = impostor_root.join(
        lookback_tauri_lib::coding_agent::assessment::sha256_hex(b"opencode"),
    );
    let codex = temp.path().join("codex");
    fs::create_dir_all(&impostor_home).unwrap();
    fs::create_dir(&codex).unwrap();
    for path in [&impostor_root, &impostor_home, &codex] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let mut configured = profile("opencode");
    configured.mount_allowed_roots = vec![temp.path().to_path_buf()];
    configured.persistent_mounts = vec![PersistentMountSettings {
        host_path: impostor_home,
        guest_path: configured.container_home.clone(),
        read_only: false,
        executable: false,
    }];
    configured.host_configuration = Some(HostAgentConfigurationSettings {
        codex_directory: Some(codex),
        claude_directory: None,
        claude_settings_file: None,
    });

    assert!(configured.to_agent_profile().is_err());
}

#[cfg(unix)]
#[test]
fn host_agent_configuration_rejects_missing_type_escape_overlap_and_full_home_mounts() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let codex = root.join("codex");
    let claude = root.join("claude");
    fs::create_dir(&codex).unwrap();
    fs::create_dir(&claude).unwrap();
    for path in [&codex, &claude] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let mut base = profile("opencode");
    base.mount_allowed_roots = vec![root.to_path_buf()];
    base.host_configuration = Some(HostAgentConfigurationSettings {
        codex_directory: Some(codex.clone()),
        claude_directory: Some(claude.clone()),
        claude_settings_file: None,
    });
    assert!(base.to_agent_profile_with_managed_home_root(root).is_ok());

    for invalid in [
        HostAgentConfigurationSettings {
            codex_directory: Some(root.join("missing")),
            claude_directory: None,
            claude_settings_file: None,
        },
        HostAgentConfigurationSettings {
            codex_directory: Some(root.join("file")),
            claude_directory: None,
            claude_settings_file: None,
        },
        HostAgentConfigurationSettings {
            codex_directory: Some(codex.clone()),
            claude_directory: Some(codex.clone()),
            claude_settings_file: None,
        },
        HostAgentConfigurationSettings {
            codex_directory: Some(root.join("..")),
            claude_directory: None,
            claude_settings_file: None,
        },
    ] {
        let mut configured = base.clone();
        if invalid.codex_directory.as_deref() == Some(root.join("file").as_path()) {
            fs::write(root.join("file"), b"file").unwrap();
        }
        configured.host_configuration = Some(invalid);
        assert!(
            configured
                .to_agent_profile_with_managed_home_root(root)
                .is_err()
        );
    }

    let symlink = root.join("codex-link");
    std::os::unix::fs::symlink(&codex, &symlink).unwrap();
    let mut symlinked = base.clone();
    symlinked.host_configuration = Some(HostAgentConfigurationSettings {
        codex_directory: Some(symlink),
        claude_directory: None,
        claude_settings_file: None,
    });
    assert!(
        symlinked
            .to_agent_profile_with_managed_home_root(root)
            .is_err()
    );

    let directory_as_file = root.join("settings-directory");
    fs::create_dir(&directory_as_file).unwrap();
    let mut wrong_type = base.clone();
    wrong_type.host_configuration = Some(HostAgentConfigurationSettings {
        codex_directory: None,
        claude_directory: None,
        claude_settings_file: Some(directory_as_file),
    });
    assert!(
        wrong_type
            .to_agent_profile_with_managed_home_root(root)
            .is_err()
    );

    let mut full_home = base;
    full_home.persistent_mounts = vec![PersistentMountSettings {
        host_path: claude,
        guest_path: "/home/agent".into(),
        read_only: false,
        executable: false,
    }];
    assert!(
        full_home
            .to_agent_profile_with_managed_home_root(root)
            .is_err()
    );

    let managed_root = tempfile::tempdir().unwrap();
    let profile_homes = managed_root.path().join("profile-homes");
    let managed_home = profile_homes.join(
        lookback_tauri_lib::coding_agent::assessment::sha256_hex(b"managed-sources"),
    );
    let sibling_codex = managed_root.path().join("codex");
    let sibling_claude = managed_root.path().join("claude");
    let sibling_settings = managed_root.path().join("claude.json");
    let shared_home = managed_root.path().join("profile-shared-homes").join(
        lookback_tauri_lib::coding_agent::assessment::sha256_hex(b"managed-sources"),
    );
    fs::create_dir_all(&managed_home).unwrap();
    fs::create_dir_all(&shared_home).unwrap();
    fs::create_dir(&sibling_codex).unwrap();
    fs::create_dir(&sibling_claude).unwrap();
    fs::write(&sibling_settings, b"fixture").unwrap();
    for path in [
        &profile_homes,
        &managed_home,
        &shared_home,
        &sibling_codex,
        &sibling_claude,
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::set_permissions(&sibling_settings, fs::Permissions::from_mode(0o600)).unwrap();
    let mut sibling_selection = profile("managed-sources");
    sibling_selection.mount_allowed_roots = vec![managed_home.clone()];
    sibling_selection.persistent_mounts = vec![PersistentMountSettings {
        host_path: managed_home.clone(),
        guest_path: sibling_selection.container_home.clone(),
        read_only: false,
        executable: false,
    }];
    sibling_selection.host_configuration = Some(HostAgentConfigurationSettings {
        codex_directory: Some(sibling_codex.clone()),
        claude_directory: Some(sibling_claude.clone()),
        claude_settings_file: Some(sibling_settings.clone()),
    });
    let original_roots = sibling_selection.mount_allowed_roots.clone();
    let original_mounts = sibling_selection.persistent_mounts.clone();
    let effective = sibling_selection
        .to_agent_profile_with_managed_home_root(&profile_homes)
        .unwrap();
    assert!(
        !effective
            .mount_allowed_roots
            .iter()
            .any(|root| root == &managed_home)
    );
    let overlay = effective.managed_home_overlay.as_ref().unwrap();
    assert!(
        overlay
            .children
            .iter()
            .any(|child| child.is_codex_directory(&sibling_codex))
    );
    assert!(
        overlay
            .children
            .iter()
            .any(|child| child.is_claude_directory(&sibling_claude))
    );
    assert!(
        !effective
            .persistent_mounts
            .iter()
            .any(|mount| mount.host_path == sibling_settings)
    );
    fs::remove_dir(&managed_home).unwrap();
    effective
        .validate_and_snapshot(&HostResourceLimits {
            cpu_millis: 8000,
            memory_mib: 8192,
            root_disk_mib: 16384,
            max_parallel_runs: 8,
        })
        .unwrap();
    assert_eq!(sibling_selection.mount_allowed_roots, original_roots);
    assert_eq!(sibling_selection.persistent_mounts, original_mounts);
}

#[test]
fn host_agent_configuration_keeps_original_mounts_and_disables_private_home_preparation() {
    let temp = tempfile::tempdir().unwrap();
    let settings_path = temp.path().join("settings.json");
    let store = CodingAgentSettingsStore::new(&settings_path);
    let mut initial = empty_settings();
    let mut configured = profile("opencode");
    let managed_home = temp.path().join("profile-homes").join(
        lookback_tauri_lib::coding_agent::assessment::sha256_hex(b"opencode"),
    );
    configured.mount_allowed_roots = vec![managed_home.clone()];
    configured.persistent_mounts = vec![PersistentMountSettings {
        host_path: managed_home,
        guest_path: configured.container_home.clone(),
        read_only: false,
        executable: false,
    }];
    configured.host_configuration = Some(HostAgentConfigurationSettings {
        codex_directory: None,
        claude_directory: None,
        claude_settings_file: None,
    });
    // An explicit object selects shared mode even when no host paths are selected.
    initial.profiles.push(configured);
    let saved = store.compare_and_swap(0, initial, &HashSet::new()).unwrap();
    assert!(
        store
            .prepare_profile_home(saved.revision, "opencode", &HashSet::new())
            .is_err()
    );
    assert!(
        saved
            .profile_mount_availability_with_managed_home_root(&temp.path().join("profile-homes"))
            .first()
            .is_some_and(|availability| availability.available)
    );

    let mut initial = empty_settings();
    let selected_store = CodingAgentSettingsStore::new(temp.path().join("settings-selected.json"));
    let mut configured = profile("opencode");
    let selected = temp.path().join("codex");
    fs::create_dir(&selected).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&selected, fs::Permissions::from_mode(0o700)).unwrap();
    }
    configured.mount_allowed_roots = vec![temp.path().to_path_buf()];
    configured.host_configuration = Some(HostAgentConfigurationSettings {
        codex_directory: Some(selected),
        claude_directory: None,
        claude_settings_file: None,
    });
    initial.profiles.push(configured);
    let saved = selected_store
        .compare_and_swap(0, initial, &HashSet::new())
        .unwrap();
    assert!(
        selected_store
            .prepare_profile_home(saved.revision, "opencode", &HashSet::new())
            .is_err()
    );
    let loaded = selected_store.load().unwrap();
    assert_eq!(loaded, saved);
    let mut private_mode = loaded.clone();
    private_mode.profiles[0].host_configuration = None;
    let restored = selected_store
        .compare_and_swap(loaded.revision, private_mode, &HashSet::new())
        .unwrap();
    assert_eq!(
        restored.profiles[0].persistent_mounts,
        loaded.profiles[0].persistent_mounts
    );
}

#[cfg(unix)]
#[test]
fn host_shared_configuration_uses_writable_overlay_and_imports_claude_settings_once() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let temp = tempfile::tempdir().unwrap();
    let settings_path = temp.path().join("settings.json");
    let store = CodingAgentSettingsStore::new(&settings_path);
    let source_root = temp.path().join("host-home");
    let codex = source_root.join(".codex");
    let claude = source_root.join(".claude");
    let source_settings = source_root.join(".claude.json");
    fs::create_dir_all(&codex).unwrap();
    fs::create_dir(&claude).unwrap();
    fs::write(&source_settings, b"source-cli-state").unwrap();
    for path in [&source_root, &codex, &claude] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::set_permissions(&source_settings, fs::Permissions::from_mode(0o644)).unwrap();

    let mut settings = empty_settings();
    let mut configured = profile("shared-overlay");
    configured.container_home = "/custom/agent-home".into();
    configured.mount_allowed_roots = vec![source_root.clone()];
    configured.host_configuration = Some(HostAgentConfigurationSettings {
        codex_directory: Some(codex.clone()),
        claude_directory: Some(claude.clone()),
        claude_settings_file: Some(source_settings.clone()),
    });
    settings.profiles.push(configured);
    let saved = store
        .compare_and_swap(0, settings, &HashSet::new())
        .unwrap();

    let profile_homes = temp.path().join("profile-homes");
    let shared_home = temp.path().join("profile-shared-homes").join(
        lookback_tauri_lib::coding_agent::assessment::sha256_hex(b"shared-overlay"),
    );
    assert_eq!(
        saved.profiles[0]
            .host_configuration
            .as_ref()
            .unwrap()
            .claude_settings_file
            .as_deref(),
        Some(source_settings.as_path())
    );
    assert!(shared_home.is_dir());
    assert_eq!(fs::metadata(&shared_home).unwrap().uid(), unsafe {
        libc::geteuid()
    });
    assert_eq!(fs::metadata(&shared_home).unwrap().mode() & 0o777, 0o700);
    let imported = shared_home.join(".claude.json");
    assert_eq!(fs::read(&imported).unwrap(), b"source-cli-state");
    assert_eq!(fs::metadata(&imported).unwrap().mode() & 0o777, 0o600);

    let runtime = saved.profiles[0]
        .to_agent_profile_with_managed_home_root(&profile_homes)
        .unwrap();
    let overlay = runtime.managed_home_overlay.as_ref().unwrap();
    assert_eq!(overlay.parent.host_path, shared_home);
    assert_eq!(overlay.parent.guest_path, "/custom/agent-home");
    assert!(!overlay.parent.read_only);
    assert!(!overlay.parent.executable);
    assert_eq!(overlay.children.len(), 2);
    assert!(
        overlay
            .children
            .iter()
            .any(|child| child.is_codex_directory(&codex))
    );
    assert!(
        overlay
            .children
            .iter()
            .any(|child| child.is_claude_directory(&claude))
    );
    assert!(
        !runtime
            .persistent_mounts
            .iter()
            .any(|mount| mount.guest_path.ends_with(".claude.json"))
    );

    fs::write(&imported, b"cli-updated-state").unwrap();
    let mut edited = saved.clone();
    edited.profiles[0].display_name = "Edited".into();
    store
        .compare_and_swap(saved.revision, edited, &HashSet::new())
        .unwrap();
    assert_eq!(fs::read(&imported).unwrap(), b"cli-updated-state");

    let replacement = source_root.join("replacement.json");
    fs::write(&replacement, b"replacement-source").unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o644)).unwrap();
    let current = store.load().unwrap();
    let mut changed_source = current.clone();
    changed_source.profiles[0]
        .host_configuration
        .as_mut()
        .unwrap()
        .claude_settings_file = Some(replacement.clone());
    let changed_source = store
        .compare_and_swap(current.revision, changed_source, &HashSet::new())
        .unwrap();
    assert_eq!(fs::read(&imported).unwrap(), b"replacement-source");

    let unsafe_permissions = source_root.join("unsafe-permissions.json");
    fs::write(&unsafe_permissions, b"unsafe-source").unwrap();
    fs::set_permissions(&unsafe_permissions, fs::Permissions::from_mode(0o664)).unwrap();
    let mut invalid_permissions = changed_source.clone();
    invalid_permissions.profiles[0]
        .host_configuration
        .as_mut()
        .unwrap()
        .claude_settings_file = Some(unsafe_permissions);
    assert!(
        store
            .compare_and_swap(
                changed_source.revision,
                invalid_permissions,
                &HashSet::new()
            )
            .is_err()
    );
    assert_eq!(fs::read(&imported).unwrap(), b"replacement-source");

    let invalid_source = source_root.join("invalid-source");
    fs::create_dir(&invalid_source).unwrap();
    fs::set_permissions(&invalid_source, fs::Permissions::from_mode(0o700)).unwrap();
    let mut invalid_update = changed_source.clone();
    invalid_update.profiles[0]
        .host_configuration
        .as_mut()
        .unwrap()
        .claude_settings_file = Some(invalid_source);
    assert!(
        store
            .compare_and_swap(changed_source.revision, invalid_update, &HashSet::new())
            .is_err()
    );
    assert_eq!(fs::read(&imported).unwrap(), b"replacement-source");

    let mut stale = changed_source.clone();
    stale.profiles[0].display_name = "stale".into();
    assert!(matches!(
        store.compare_and_swap(saved.revision, stale, &HashSet::new()),
        Err(SettingsError::RevisionConflict { .. })
    ));
    assert_eq!(fs::read(&imported).unwrap(), b"replacement-source");

    fs::remove_file(replacement).unwrap();
    let loaded = store.load().unwrap();
    loaded.profiles[0]
        .to_agent_profile_with_managed_home_root(&profile_homes)
        .unwrap()
        .validate_and_snapshot(&HostResourceLimits {
            cpu_millis: 8000,
            memory_mib: 8192,
            root_disk_mib: 16384,
            max_parallel_runs: 8,
        })
        .unwrap();
}

#[cfg(unix)]
#[test]
fn settings_cas_failure_rolls_back_claude_import_for_existing_and_new_destinations() {
    use std::os::unix::fs::PermissionsExt;

    let make_oversized =
        |mut settings: CodingAgentSettings, target_profile_id: &str, source: &std::path::Path| {
            settings
                .profiles
                .iter_mut()
                .find(|profile| profile.profile_id == target_profile_id)
                .unwrap()
                .host_configuration
                .as_mut()
                .unwrap()
                .claude_settings_file = Some(source.to_path_buf());
            for profile_index in 0..20 {
                let mut filler = profile(&format!("rollback-filler-{profile_index}"));
                filler.allowed_image_refs = vec![IMAGE.into()];
                for image_index in 0..63 {
                    filler.allowed_image_refs.push(format!(
                        "registry.example/{profile_index:02}-{image_index:02}-{}",
                        "x".repeat(980)
                    ));
                }
                settings.profiles.push(filler);
            }
            settings
        };

    let setup_sources = |root: &std::path::Path| {
        let host_root = root.join("host-home");
        let source_one = host_root.join("first.json");
        let source_two = host_root.join("second.json");
        fs::create_dir(&host_root).unwrap();
        fs::write(&source_one, b"first-source").unwrap();
        fs::write(&source_two, b"second-source").unwrap();
        fs::set_permissions(&host_root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&source_one, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&source_two, fs::Permissions::from_mode(0o644)).unwrap();
        (source_one, source_two)
    };

    let existing_root = tempfile::tempdir().unwrap();
    let (existing_source_one, existing_source_two) = setup_sources(existing_root.path());
    let existing_store = CodingAgentSettingsStore::new(existing_root.path().join("settings.json"));
    let mut existing_initial = empty_settings();
    let mut existing_profile = profile("rollback-existing");
    existing_profile.host_configuration = Some(HostAgentConfigurationSettings {
        codex_directory: None,
        claude_directory: None,
        claude_settings_file: Some(existing_source_one.clone()),
    });
    existing_initial.profiles.push(existing_profile);
    let existing_saved = existing_store
        .compare_and_swap(0, existing_initial, &HashSet::new())
        .unwrap();
    let existing_destination = existing_root
        .path()
        .join("profile-shared-homes")
        .join(lookback_tauri_lib::coding_agent::assessment::sha256_hex(
            b"rollback-existing",
        ))
        .join(".claude.json");
    assert_eq!(fs::read(&existing_destination).unwrap(), b"first-source");

    let oversized_existing = make_oversized(
        existing_saved.clone(),
        "rollback-existing",
        &existing_source_two,
    );
    let existing_error = existing_store
        .compare_and_swap(existing_saved.revision, oversized_existing, &HashSet::new())
        .expect_err("the oversized settings write must fail after importing the new source");
    assert!(matches!(
        existing_error,
        SettingsError::Invalid(message)
            if message == "settings exceed the configured size limit"
    ));
    assert_eq!(
        existing_store.load().unwrap().revision,
        existing_saved.revision
    );
    assert_eq!(fs::read(&existing_destination).unwrap(), b"first-source");

    let new_root = tempfile::tempdir().unwrap();
    let (new_source_one, _) = setup_sources(new_root.path());
    let new_store = CodingAgentSettingsStore::new(new_root.path().join("settings.json"));
    let mut new_initial = empty_settings();
    new_initial.profiles.push(profile("rollback-new"));
    let new_saved = new_store
        .compare_and_swap(0, new_initial, &HashSet::new())
        .unwrap();
    let mut new_with_source = new_saved.clone();
    new_with_source.profiles[0].host_configuration = Some(HostAgentConfigurationSettings {
        codex_directory: None,
        claude_directory: None,
        claude_settings_file: Some(new_source_one.clone()),
    });
    let oversized_new = make_oversized(new_with_source, "rollback-new", &new_source_one);
    let new_destination = new_root
        .path()
        .join("profile-shared-homes")
        .join(lookback_tauri_lib::coding_agent::assessment::sha256_hex(
            b"rollback-new",
        ))
        .join(".claude.json");
    let new_error = new_store
        .compare_and_swap(new_saved.revision, oversized_new, &HashSet::new())
        .expect_err("the oversized settings write must fail after creating the new destination");
    assert!(matches!(
        new_error,
        SettingsError::Invalid(message)
            if message == "settings exceed the configured size limit"
    ));
    assert_eq!(new_store.load().unwrap().revision, new_saved.revision);
    assert!(!new_destination.exists());
}

#[test]
fn legacy_fractional_cpu_profiles_can_be_read_and_explicitly_repaired_without_rounding() {
    let limits = HostResourceLimits {
        cpu_millis: 8000,
        memory_mib: 8192,
        root_disk_mib: 16384,
        max_parallel_runs: 8,
    };
    for cpu in [1, 1500] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let store = CodingAgentSettingsStore::new(path.clone());
        let mut old = empty_settings();
        old.profiles.push(profile("legacy"));
        old.profiles[0].resources.cpu_millis = cpu;
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let loaded = store.load().expect("legacy settings remain editable");
        assert_eq!(loaded.profiles[0].resources.cpu_millis, cpu);
        assert!(
            loaded.profiles[0]
                .to_agent_profile()
                .unwrap()
                .validate_and_snapshot(&limits)
                .is_err()
        );
        assert!(
            store
                .compare_and_swap(loaded.revision, loaded.clone(), &HashSet::new())
                .is_err()
        );
        let mut repaired = loaded;
        repaired.profiles[0].resources.cpu_millis = 2000;
        let saved = store
            .compare_and_swap(repaired.revision, repaired, &HashSet::new())
            .unwrap();
        let snapshot = saved.profiles[0]
            .to_agent_profile()
            .unwrap()
            .validate_and_snapshot(&limits)
            .unwrap();
        assert_eq!(snapshot.resources.cpu_millis, 2000);
    }
}

#[test]
fn legacy_profile_without_network_field_stays_disabled_after_loading_and_snapshotting() {
    use lookback_tauri_lib::coding_agent::profile::NetworkAccess;
    let mut wire = serde_json::to_value(profile("legacy")).unwrap();
    wire.as_object_mut().unwrap().remove("networkAccess");
    let loaded: AgentProfileSettings = serde_json::from_value(wire).unwrap();
    assert_eq!(loaded.network_access, NetworkAccess::Disabled);
    assert_eq!(
        loaded.to_agent_profile().unwrap().network_access,
        NetworkAccess::Disabled
    );
}

#[test]
fn runtime_environment_survives_settings_roundtrip_and_rejects_unsafe_values() {
    let mut legacy_settings = empty_settings();
    legacy_settings.profiles.push(profile("legacy"));
    let mut legacy_wire = serde_json::to_value(&legacy_settings).unwrap();
    legacy_wire["profiles"][0]
        .as_object_mut()
        .unwrap()
        .remove("runtimeEnvironment");
    let legacy_decoded: CodingAgentSettings = serde_json::from_value(legacy_wire.clone()).unwrap();
    assert!(legacy_decoded.profiles[0].runtime_environment.is_empty());
    assert_eq!(serde_json::to_value(legacy_decoded).unwrap(), legacy_wire);

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settings.json");
    let store = CodingAgentSettingsStore::new(&path);
    let mut proposed = empty_settings();
    let mut configured = profile("opencode");
    configured.runtime_environment = BTreeMap::from([
        ("CARGO_HOME".into(), "/home/codespace/.cargo".into()),
        ("RUSTUP_HOME".into(), "/home/codespace/.rustup".into()),
    ]);
    proposed.profiles.push(configured);

    let saved = store
        .compare_and_swap(0, proposed, &HashSet::new())
        .unwrap();
    assert_eq!(CodingAgentSettingsStore::new(&path).load().unwrap(), saved);
    let wire = serde_json::to_value(&saved).unwrap();
    assert_eq!(
        wire["profiles"][0]["runtimeEnvironment"]["CARGO_HOME"],
        "/home/codespace/.cargo"
    );
    let snapshot = saved.profiles[0]
        .to_agent_profile()
        .unwrap()
        .validate_and_snapshot(&HostResourceLimits {
            cpu_millis: 8000,
            memory_mib: 8192,
            root_disk_mib: 16384,
            max_parallel_runs: 8,
        })
        .unwrap();
    assert_eq!(
        snapshot.runtime_environment,
        saved.profiles[0].runtime_environment
    );

    for (key, value) in [
        ("HOME", "/home/codespace/.agent-home"),
        ("PATH", "/home/codespace/.bin"),
        ("CARGO_HOME", "relative/cargo"),
        ("RUSTUP_HOME", ""),
        ("CODEX_HOME", "/home/codespace/.agent-home\n"),
    ] {
        let mut invalid = saved.clone();
        invalid.profiles[0]
            .runtime_environment
            .insert(key.into(), value.into());
        assert!(
            store
                .compare_and_swap(saved.revision, invalid, &HashSet::new())
                .is_err(),
            "unsafe runtime environment entry was accepted: {key}={value:?}"
        );
    }
    assert_eq!(store.load().unwrap(), saved);
}

#[test]
fn profile_image_tag_can_move_to_another_registry_without_losing_other_settings() {
    let temp = tempfile::tempdir().unwrap();
    let store = CodingAgentSettingsStore::new(temp.path().join("settings.json"));
    let mut initial = empty_settings();
    initial.profiles.push(profile("opencode"));
    let saved = store.compare_and_swap(0, initial, &HashSet::new()).unwrap();
    let mut changed = saved.clone();
    let image = "another.example:5000/public/opencode:20261007_1";
    changed.profiles[0].image_ref = image.into();
    changed.profiles[0].allowed_image_refs = vec![image.into()];
    let saved = store
        .compare_and_swap(saved.revision, changed, &HashSet::new())
        .unwrap();
    assert_eq!(store.load().unwrap(), saved);
    assert_eq!(saved.profiles[0].image_ref, image);
    for invalid in [
        "https://another.example/repo:tag",
        "example/repo@sha256:abc",
        "../repo:tag",
        "example/repo;touch",
    ] {
        let mut changed = saved.clone();
        changed.profiles[0].image_ref = invalid.into();
        changed.profiles[0].allowed_image_refs = vec![invalid.into()];
        assert!(
            store
                .compare_and_swap(saved.revision, changed, &HashSet::new())
                .is_err()
        );
    }
    assert_eq!(store.load().unwrap(), saved);
}

#[test]
fn human_review_run_snapshot_rejects_auto_completion_and_preserves_judge_setting() {
    let snapshot = shutdown_test_snapshot(RunId::new("human-review-run").unwrap());
    let mut payload = serde_json::to_value(snapshot).unwrap();
    payload["semantic_judge_enabled"] = serde_json::json!(false);
    let snapshot: AgentRunSnapshot = serde_json::from_value(payload.clone()).unwrap();
    let normalized = snapshot.normalize_for_storage().unwrap();
    assert_eq!(
        serde_json::to_value(normalized).unwrap()["semantic_judge_enabled"],
        false
    );
    payload["auto_done"] = serde_json::json!(true);
    let invalid: AgentRunSnapshot = serde_json::from_value(payload).unwrap();
    assert!(invalid.normalize_for_storage().is_err());
}

#[test]
fn existing_settings_keep_semantic_assessment_for_approved_auto_completion() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settings.json");
    let mut payload = serde_json::to_value(empty_settings()).unwrap();
    payload
        .as_object_mut()
        .unwrap()
        .remove("semanticJudgeEnabled");
    payload["autoDoneEnabled"] = serde_json::json!(true);
    std::fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();
    let store = CodingAgentSettingsStore::new(&path);
    let loaded = store.load().unwrap();
    assert!(loaded.semantic_judge_enabled);
    assert!(loaded.auto_done_enabled);
    let saved = store
        .compare_and_swap(loaded.revision, loaded, &HashSet::new())
        .unwrap();
    assert_eq!(store.load().unwrap(), saved);
}

#[test]
fn human_review_settings_survive_reopen_and_reject_automatic_completion() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settings.json");
    let store = CodingAgentSettingsStore::new(&path);
    let mut payload = serde_json::to_value(empty_settings()).unwrap();
    payload["semanticJudgeEnabled"] = serde_json::json!(false);
    let proposed = serde_json::from_value(payload.clone()).unwrap();
    let saved = store
        .compare_and_swap(0, proposed, &HashSet::new())
        .unwrap();
    assert_eq!(CodingAgentSettingsStore::new(&path).load().unwrap(), saved);
    payload["autoDoneEnabled"] = serde_json::json!(true);
    let invalid = serde_json::from_value(payload).unwrap();
    assert!(
        store
            .compare_and_swap(saved.revision, invalid, &HashSet::new())
            .is_err()
    );
    assert_eq!(
        store.load().unwrap(),
        saved,
        "rejected updates must preserve the human review setting"
    );
}

#[test]
fn semantic_retry_settings_survive_reopen_reject_invalid_limits_and_accept_legacy_payloads() {
    use lookback_tauri_lib::coding_agent::semantic_judge::SemanticJudgePolicy;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settings.json");
    let store = CodingAgentSettingsStore::new(&path);
    let mut proposed = empty_settings();
    proposed.semantic_judge_policy = SemanticJudgePolicy {
        max_retries: 2,
        deadline_seconds: 180,
    };
    let saved = store
        .compare_and_swap(0, proposed, &HashSet::new())
        .unwrap();
    assert_eq!(CodingAgentSettingsStore::new(&path).load().unwrap(), saved);
    for policy in [
        SemanticJudgePolicy {
            max_retries: 6,
            deadline_seconds: 180,
        },
        SemanticJudgePolicy {
            max_retries: 2,
            deadline_seconds: 0,
        },
    ] {
        let mut invalid = saved.clone();
        invalid.semantic_judge_policy = policy;
        assert!(
            store
                .compare_and_swap(saved.revision, invalid, &HashSet::new())
                .is_err()
        );
        assert_eq!(store.load().unwrap(), saved);
    }
    let mut legacy = serde_json::to_value(&saved).unwrap();
    legacy
        .as_object_mut()
        .unwrap()
        .remove("semanticJudgePolicy");
    let mut decoded: CodingAgentSettings = serde_json::from_value(legacy).unwrap();
    decoded.semantic_judge_enabled = false;
    decoded.semantic_judge_policy = saved.semantic_judge_policy;
    let disabled = store
        .compare_and_swap(saved.revision, decoded, &HashSet::new())
        .unwrap();
    assert!(!disabled.semantic_judge_enabled);
    assert_eq!(disabled.semantic_judge_policy, saved.semantic_judge_policy);
}

#[test]
fn settings_survive_reopen_and_reject_a_stale_editor_revision() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("coding-agent/settings.json");
    let first = CodingAgentSettingsStore::new(&path);
    let mut proposed = empty_settings();
    proposed.profiles.push(profile("opencode"));
    proposed.active_profile_id = Some("opencode".into());

    let saved = first
        .compare_and_swap(0, proposed.clone(), &HashSet::new())
        .unwrap();
    assert_eq!(saved.revision, 1);
    assert!(!saved.auto_done_enabled);
    assert_eq!(
        CodingAgentSettingsStore::new(&path).load().unwrap(),
        saved,
        "a new app owner must read the same durable settings snapshot"
    );

    assert!(matches!(
        first.compare_and_swap(0, proposed, &HashSet::new()),
        Err(SettingsError::RevisionConflict { .. })
    ));
}

#[test]
fn changing_a_profile_advances_its_immutable_snapshot_version() {
    let temp = tempfile::tempdir().unwrap();
    let store = CodingAgentSettingsStore::new(temp.path().join("settings.json"));
    let mut proposed = empty_settings();
    proposed.profiles.push(profile("opencode"));
    let first = store
        .compare_and_swap(0, proposed.clone(), &HashSet::new())
        .unwrap();
    proposed = first.clone();
    proposed.profiles[0].display_name = "OpenCode review".into();

    let changed = store
        .compare_and_swap(first.revision, proposed, &HashSet::new())
        .unwrap();

    assert_eq!(changed.profiles[0].version, first.profiles[0].version + 1);
    assert_eq!(changed.revision, first.revision + 1);
}

#[test]
fn settings_reject_mutating_a_profile_used_by_an_unresolved_run() {
    let temp = tempfile::tempdir().unwrap();
    let store = CodingAgentSettingsStore::new(temp.path().join("settings.json"));
    let mut initial = empty_settings();
    initial.profiles.push(profile("opencode"));
    initial.active_profile_id = Some("opencode".into());
    let saved = store.compare_and_swap(0, initial, &HashSet::new()).unwrap();

    let mut changed = saved.clone();
    changed.profiles[0].display_name = "Changed while running".into();
    let unresolved_profiles = HashSet::from(["opencode".to_owned()]);

    assert!(matches!(
        store.compare_and_swap(saved.revision, changed, &unresolved_profiles),
        Err(SettingsError::ProfileInUse(profile_id)) if profile_id == "opencode"
    ));
    assert_eq!(store.load().unwrap(), saved);
}

#[test]
fn unavailable_mounts_remain_readable_and_can_be_repaired_by_cas() {
    let temp = tempfile::tempdir().unwrap();
    let missing_root = temp.path().join("workspace-that-was-moved");
    let store = CodingAgentSettingsStore::new(temp.path().join("settings.json"));
    let mut initial = empty_settings();
    let mut configured = profile("opencode");
    configured.mount_allowed_roots.push(missing_root.clone());
    configured.persistent_mounts.push(PersistentMountSettings {
        host_path: missing_root.join("project"),
        guest_path: "/workspace/project".into(),
        read_only: false,
        executable: false,
    });
    initial.profiles.push(configured);

    let saved = store
        .compare_and_swap(0, initial, &HashSet::new())
        .expect("missing live mount paths are a repairable availability issue");
    let reopened = CodingAgentSettingsStore::new(temp.path().join("settings.json"))
        .load()
        .expect("GET must retain the structural settings snapshot");
    assert_eq!(reopened, saved);
    let availability = reopened.profile_mount_availability();
    assert_eq!(availability.len(), 1);
    assert_eq!(availability[0].profile_id, "opencode");
    assert!(!availability[0].available);
    assert!(
        availability[0]
            .blockers
            .contains(&"mount_root_unavailable".into())
    );

    let mut repaired = reopened;
    repaired.profiles[0].mount_allowed_roots.clear();
    repaired.profiles[0].persistent_mounts.clear();
    let saved_repair = store
        .compare_and_swap(saved.revision, repaired, &HashSet::new())
        .expect("repair must be accepted against the previously readable revision");
    assert_eq!(saved_repair.revision, saved.revision + 1);
    assert!(saved_repair.profile_mount_availability()[0].available);
}

#[cfg(unix)]
#[test]
fn missing_host_configuration_paths_remain_readable_and_can_be_repaired_by_cas() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let store = CodingAgentSettingsStore::new(temp.path().join("settings.json"));
    let host_codex = temp.path().join("host-codex");
    fs::create_dir(&host_codex).unwrap();
    fs::set_permissions(&host_codex, fs::Permissions::from_mode(0o700)).unwrap();
    let mut initial = empty_settings();
    let mut configured = profile("opencode");
    configured.host_configuration = Some(HostAgentConfigurationSettings {
        codex_directory: Some(host_codex.clone()),
        claude_directory: None,
        claude_settings_file: None,
    });
    initial.profiles.push(configured);

    let saved = store
        .compare_and_swap(0, initial, &HashSet::new())
        .expect("a new private host path is validated before saving");
    fs::remove_dir(&host_codex).unwrap();
    let reopened = store
        .load()
        .expect("GET must retain the repairable snapshot");
    assert_eq!(reopened, saved);
    assert!(!reopened.profile_mount_availability()[0].available);

    let mut repaired = reopened;
    repaired.profiles[0].host_configuration = None;
    let saved_repair = store
        .compare_and_swap(saved.revision, repaired, &HashSet::new())
        .expect("CAS must be able to remove an unavailable host selection");
    assert_eq!(saved_repair.revision, saved.revision + 1);
    assert!(saved_repair.profile_mount_availability()[0].available);
}

#[cfg(unix)]
#[test]
fn cas_validates_new_or_changed_host_configuration_directories() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let store = CodingAgentSettingsStore::new(temp.path().join("settings.json"));
    let valid = temp.path().join("valid-codex");
    let replacement = temp.path().join("replacement-codex");
    fs::create_dir(&valid).unwrap();
    fs::create_dir(&replacement).unwrap();
    for path in [&valid, &replacement] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut initial = empty_settings();
    let mut configured = profile("host-path-validation");
    configured.host_configuration = Some(HostAgentConfigurationSettings {
        codex_directory: Some(valid.clone()),
        claude_directory: None,
        claude_settings_file: None,
    });
    initial.profiles.push(configured);
    let saved = store
        .compare_and_swap(0, initial, &HashSet::new())
        .expect("a new private host directory is valid");

    fs::remove_dir(&valid).unwrap();
    let mut unrelated = saved.clone();
    unrelated.auto_done_enabled = true;
    let saved = store
        .compare_and_swap(saved.revision, unrelated, &HashSet::new())
        .expect("an unchanged unavailable path remains repairable");

    let mut changed = saved.clone();
    changed.profiles[0]
        .host_configuration
        .as_mut()
        .unwrap()
        .codex_directory = Some(replacement.clone());
    let changed = store
        .compare_and_swap(saved.revision, changed, &HashSet::new())
        .expect("a changed private host directory is valid");

    let missing = temp.path().join("missing-codex");
    let mut invalid = changed.clone();
    invalid.profiles[0]
        .host_configuration
        .as_mut()
        .unwrap()
        .codex_directory = Some(missing);
    assert!(matches!(
        store.compare_and_swap(changed.revision, invalid, &HashSet::new()),
        Err(SettingsError::Invalid(_))
    ));

    let public = temp.path().join("public-codex");
    fs::create_dir(&public).unwrap();
    fs::set_permissions(&public, fs::Permissions::from_mode(0o775)).unwrap();
    let mut invalid = changed.clone();
    invalid.profiles[0]
        .host_configuration
        .as_mut()
        .unwrap()
        .codex_directory = Some(public);
    assert!(matches!(
        store.compare_and_swap(changed.revision, invalid, &HashSet::new()),
        Err(SettingsError::Invalid(_))
    ));

    let file = temp.path().join("file-codex");
    fs::write(&file, b"not a directory").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let mut invalid = changed.clone();
    invalid.profiles[0]
        .host_configuration
        .as_mut()
        .unwrap()
        .codex_directory = Some(file);
    assert!(matches!(
        store.compare_and_swap(changed.revision, invalid, &HashSet::new()),
        Err(SettingsError::Invalid(_))
    ));

    let symlink_target = temp.path().join("symlink-target-codex");
    let symlink = temp.path().join("symlink-codex");
    fs::create_dir(&symlink_target).unwrap();
    fs::set_permissions(&symlink_target, fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&symlink_target, &symlink).unwrap();
    let mut invalid = changed;
    invalid.profiles[0]
        .host_configuration
        .as_mut()
        .unwrap()
        .codex_directory = Some(symlink);
    assert!(matches!(
        store.compare_and_swap(invalid.revision, invalid, &HashSet::new()),
        Err(SettingsError::Invalid(_))
    ));
}

#[test]
fn an_unresolved_profile_is_immutable_but_an_unrelated_profile_is_editable() {
    let temp = tempfile::tempdir().unwrap();
    let store = CodingAgentSettingsStore::new(temp.path().join("settings.json"));
    let mut initial = empty_settings();
    initial
        .profiles
        .extend([profile("active-run"), profile("other")]);
    let saved = store.compare_and_swap(0, initial, &HashSet::new()).unwrap();
    let mut edited = saved.clone();
    edited.profiles[1].display_name = "Updated unrelated profile".into();

    let updated = store
        .compare_and_swap(
            saved.revision,
            edited,
            &HashSet::from(["active-run".to_owned()]),
        )
        .expect("editing an unrelated profile must not block recovery");

    assert_eq!(updated.profiles[0], saved.profiles[0]);
    assert_eq!(updated.profiles[1].version, saved.profiles[1].version + 1);
}

#[test]
fn settings_reject_unknown_fields_and_path_escape_mounts() {
    let mut unknown_value = serde_json::to_value(CodingAgentSettings::default()).unwrap();
    unknown_value["unexpected"] = serde_json::Value::Bool(true);
    let unknown: Result<CodingAgentSettings, _> = serde_json::from_value(unknown_value);
    assert!(
        unknown.is_err(),
        "unknown public settings fields fail closed"
    );

    let temp = tempfile::tempdir().unwrap();
    let store = CodingAgentSettingsStore::new(temp.path().join("settings.json"));
    let mut proposed = empty_settings();
    let mut invalid = profile("opencode");
    invalid.mount_allowed_roots.push(temp.path().to_path_buf());
    invalid.persistent_mounts.push(PersistentMountSettings {
        host_path: temp.path().join("../outside"),
        guest_path: "/workspace".into(),
        read_only: false,
        executable: false,
    });
    proposed.profiles.push(invalid);

    assert!(matches!(
        store.compare_and_swap(0, proposed, &HashSet::new()),
        Err(SettingsError::Invalid(_))
    ));
    assert!(!temp.path().join("settings.json").exists());
}

#[test]
fn malformed_settings_are_reported_without_erasing_the_previous_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settings.json");
    let original = b"{not valid settings";
    std::fs::write(&path, original).unwrap();

    assert!(CodingAgentSettingsStore::new(&path).load().is_err());
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn settings_and_lock_are_private_files() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("coding-agent/settings.json");
    let store = CodingAgentSettingsStore::new(&path);
    store
        .compare_and_swap(0, empty_settings(), &HashSet::new())
        .unwrap();

    let settings_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    let lock_mode = std::fs::metadata(path.with_extension("json.lock"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(settings_mode, 0o600);
    assert_eq!(lock_mode, 0o600);
}

#[test]
fn concurrent_settings_editors_cannot_both_commit_the_same_revision() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settings.json");
    let store = CodingAgentSettingsStore::new(&path);
    let mut seed = empty_settings();
    seed.profiles.push(profile("opencode"));
    let seed = store.compare_and_swap(0, seed, &HashSet::new()).unwrap();
    let contenders = (0..2)
        .map(|index| {
            let store = CodingAgentSettingsStore::new(&path);
            let mut proposal = seed.clone();
            proposal.profiles[0].display_name = format!("OpenCode {index}");
            std::thread::spawn(move || {
                store.compare_and_swap(seed.revision, proposal, &HashSet::new())
            })
        })
        .collect::<Vec<_>>();
    let results = contenders
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    assert_eq!(
        CodingAgentSettingsStore::new(path).load().unwrap().revision,
        seed.revision + 1
    );
}

#[test]
fn run_owner_is_bound_to_one_window_and_cannot_be_stolen() {
    let owners = CodingAgentRunOwnerRegistry::default();
    owners.claim("run-1", "main").unwrap();

    assert!(owners.is_owned_by("run-1", "main"));
    assert!(!owners.is_owned_by("run-1", "settings"));
    assert!(owners.claim("run-1", "settings").is_err());
    assert!(owners.claim("run-1", "main").is_ok());
}

#[test]
fn dropping_a_terminal_view_does_not_cancel_the_app_owned_stream() {
    let owners = TerminalOwnerRegistry::default();
    owners
        .install(
            ExecutionIdentity {
                run_id: "run-1".into(),
                attempt_id: "attempt-1".into(),
                workflow_execution_id: "workflow-1".into(),
                parent_job_id: "parent-1".into(),
                child_sandbox_job_id: "child-1".into(),
                worker_id: 1,
                sandbox_job_id: "sandbox-1".into(),
            },
            std::sync::Arc::new(UnusedTerminalControl),
            4,
        )
        .unwrap();
    let subscription = owners.subscribe("run-1", "attempt-1").unwrap();
    drop(subscription);

    assert_eq!(owners.owner_count(), 1);
    assert!(owners.identity("run-1", "attempt-1").is_some());
}

#[test]
fn constructed_tray_with_unverified_host_is_offered_as_a_disabled_background_choice() {
    struct ConstructedTrayIcon;

    let constructed_icon = ConstructedTrayIcon;
    let tray_availability =
        tray_availability_for_icon(Some(&constructed_icon), TrayAvailability::Unverified);

    let lifecycle = WindowLifecycle::default();
    let request = lifecycle
        .begin_close_request("main", tray_availability)
        .expect("a first close request should be presented");
    assert!(!request.background_available);
    assert_eq!(
        request.background_unavailable_reason,
        Some(BackgroundUnavailableReason::Unverified)
    );
    assert!(lifecycle.acknowledge_close_request(request.request_id));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_tray_icon_construction_alone_does_not_verify_a_restorable_host() {
    assert_eq!(
        platform_tray_host_availability(None),
        TrayAvailability::Unverified
    );
}

#[test]
fn absent_tray_icon_is_unavailable_even_when_the_platform_can_host_one() {
    assert_eq!(
        tray_availability_for_icon::<()>(None, TrayAvailability::VerifiedRestorable),
        TrayAvailability::Unavailable
    );
}

struct FakeShutdownOperations {
    run: std::sync::Mutex<CodingAgentShutdownRun>,
    unknown_persisted: AtomicBool,
    persistence_fails: bool,
    cancel_completes: bool,
}

#[async_trait::async_trait]
impl CodingAgentShutdownOperations for FakeShutdownOperations {
    fn recoverable_runs(&self) -> Result<Vec<CodingAgentShutdownRun>, CodingAgentShutdownError> {
        Ok(vec![self.run.lock().unwrap().clone()])
    }

    async fn cancel(&self, _run_id: &str) -> Result<(), CodingAgentShutdownError> {
        if self.cancel_completes {
            let mut run = self.run.lock().unwrap();
            run.state = lookback_tauri_lib::coding_agent::RunState::Cancelled;
            run.revision += 1;
            Ok(())
        } else {
            std::future::pending().await
        }
    }

    async fn reconcile(&self, _run_id: &str) -> Result<(), CodingAgentShutdownError> {
        if self.cancel_completes {
            Ok(())
        } else {
            std::future::pending().await
        }
    }

    fn current_run(
        &self,
        _run_id: &str,
    ) -> Result<CodingAgentShutdownRun, CodingAgentShutdownError> {
        Ok(self.run.lock().unwrap().clone())
    }

    fn persist_unknown(
        &self,
        run: &CodingAgentShutdownRun,
    ) -> Result<CodingAgentShutdownRun, CodingAgentShutdownError> {
        if self.persistence_fails {
            return Err(CodingAgentShutdownError::Storage);
        }
        let mut current = self.run.lock().unwrap();
        assert_eq!(current.revision, run.revision);
        current.state = lookback_tauri_lib::coding_agent::RunState::Unknown;
        current.revision += 1;
        self.unknown_persisted.store(true, Ordering::SeqCst);
        Ok(current.clone())
    }
}

#[tokio::test]
async fn shutdown_records_unknown_after_unconfirmed_cancel_then_allows_exit() {
    let operations = FakeShutdownOperations {
        run: std::sync::Mutex::new(CodingAgentShutdownRun {
            run_id: "run-unresponsive".into(),
            revision: 4,
            state: lookback_tauri_lib::coding_agent::RunState::Running,
        }),
        unknown_persisted: AtomicBool::new(false),
        persistence_fails: false,
        cancel_completes: false,
    };

    let outcome = reconcile_coding_agent_shutdown_runs(
        &operations,
        tokio::time::Instant::now() + std::time::Duration::from_millis(20),
    )
    .await
    .expect("a durable Unknown state is sufficient to continue full quit");

    assert_eq!(outcome.cancellation_attempts, 1);
    assert_eq!(
        outcome.unconfirmed_run_ids,
        vec!["run-unresponsive".to_owned()]
    );
    assert!(operations.unknown_persisted.load(Ordering::SeqCst));
    assert_eq!(
        operations.run.lock().unwrap().state,
        lookback_tauri_lib::coding_agent::RunState::Unknown
    );
}

#[tokio::test]
async fn shutdown_refuses_to_stop_when_unknown_state_cannot_be_persisted() {
    let operations = FakeShutdownOperations {
        run: std::sync::Mutex::new(CodingAgentShutdownRun {
            run_id: "run-storage-failure".into(),
            revision: 7,
            state: lookback_tauri_lib::coding_agent::RunState::Running,
        }),
        unknown_persisted: AtomicBool::new(false),
        persistence_fails: true,
        cancel_completes: false,
    };

    assert!(
        reconcile_coding_agent_shutdown_runs(
            &operations,
            tokio::time::Instant::now() + std::time::Duration::from_millis(20),
        )
        .await
        .is_err()
    );
    assert!(!operations.unknown_persisted.load(Ordering::SeqCst));
}

#[tokio::test]
async fn shutdown_stops_sidecars_only_after_unconfirmed_run_is_durably_unknown() {
    use lookback_tauri_lib::commands::reconcile_coding_agent_shutdown_then_stop;

    let operations = FakeShutdownOperations {
        run: std::sync::Mutex::new(CodingAgentShutdownRun {
            run_id: "run-stop-order".into(),
            revision: 2,
            state: lookback_tauri_lib::coding_agent::RunState::Running,
        }),
        unknown_persisted: AtomicBool::new(false),
        persistence_fails: false,
        cancel_completes: false,
    };
    let stop_called = AtomicBool::new(false);

    let result = reconcile_coding_agent_shutdown_then_stop(
        &operations,
        tokio::time::Instant::now() + std::time::Duration::from_millis(20),
        || async {
            assert!(operations.unknown_persisted.load(Ordering::SeqCst));
            stop_called.store(true, Ordering::SeqCst);
            Ok(())
        },
    )
    .await;

    assert!(result.is_ok());
    assert!(stop_called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn shutdown_does_not_stop_sidecars_if_unknown_persistence_fails() {
    use lookback_tauri_lib::commands::reconcile_coding_agent_shutdown_then_stop;

    let operations = FakeShutdownOperations {
        run: std::sync::Mutex::new(CodingAgentShutdownRun {
            run_id: "run-stop-failure".into(),
            revision: 3,
            state: lookback_tauri_lib::coding_agent::RunState::Running,
        }),
        unknown_persisted: AtomicBool::new(false),
        persistence_fails: true,
        cancel_completes: false,
    };
    let stop_called = AtomicBool::new(false);

    let result = reconcile_coding_agent_shutdown_then_stop(
        &operations,
        tokio::time::Instant::now() + std::time::Duration::from_millis(20),
        || async {
            stop_called.store(true, Ordering::SeqCst);
            Ok(())
        },
    )
    .await;

    assert!(result.is_err());
    assert!(!stop_called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn shutdown_preserves_confirmed_cancellation_without_recording_unknown() {
    let operations = FakeShutdownOperations {
        run: std::sync::Mutex::new(CodingAgentShutdownRun {
            run_id: "run-cancelled".into(),
            revision: 5,
            state: lookback_tauri_lib::coding_agent::RunState::Running,
        }),
        unknown_persisted: AtomicBool::new(false),
        persistence_fails: false,
        cancel_completes: true,
    };

    let outcome = reconcile_coding_agent_shutdown_runs(
        &operations,
        tokio::time::Instant::now() + std::time::Duration::from_secs(1),
    )
    .await
    .expect("a confirmed terminal cancellation permits quit");

    assert!(outcome.unconfirmed_run_ids.is_empty());
    assert_eq!(
        operations.run.lock().unwrap().state,
        lookback_tauri_lib::coding_agent::RunState::Cancelled
    );
    assert!(!operations.unknown_persisted.load(Ordering::SeqCst));
}

struct ConcurrentStoreShutdownOperations<'a> {
    observed_store: &'a RunStore,
    competing_store: &'a RunStore,
    run_id: RunId,
}

#[async_trait::async_trait]
impl CodingAgentShutdownOperations for ConcurrentStoreShutdownOperations<'_> {
    fn recoverable_runs(&self) -> Result<Vec<CodingAgentShutdownRun>, CodingAgentShutdownError> {
        self.observed_store
            .recoverable_runs()
            .map(|runs| {
                runs.into_iter()
                    .map(|run| CodingAgentShutdownRun {
                        run_id: run.snapshot.run_id.to_string(),
                        revision: run.revision,
                        state: run.state,
                    })
                    .collect()
            })
            .map_err(|_| CodingAgentShutdownError::Storage)
    }

    async fn cancel(&self, _run_id: &str) -> Result<(), CodingAgentShutdownError> {
        std::future::pending().await
    }

    async fn reconcile(&self, _run_id: &str) -> Result<(), CodingAgentShutdownError> {
        std::future::pending().await
    }

    fn current_run(
        &self,
        _run_id: &str,
    ) -> Result<CodingAgentShutdownRun, CodingAgentShutdownError> {
        self.observed_store
            .get_run(&self.run_id)
            .map_err(|_| CodingAgentShutdownError::Storage)?
            .map(|run| CodingAgentShutdownRun {
                run_id: run.snapshot.run_id.to_string(),
                revision: run.revision,
                state: run.state,
            })
            .ok_or(CodingAgentShutdownError::RunUnavailable)
    }

    fn persist_unknown(
        &self,
        run: &CodingAgentShutdownRun,
    ) -> Result<CodingAgentShutdownRun, CodingAgentShutdownError> {
        let terminal = self
            .competing_store
            .get_run(&self.run_id)
            .map_err(|_| CodingAgentShutdownError::Storage)?
            .ok_or(CodingAgentShutdownError::RunUnavailable)?;
        self.competing_store
            .transition(&self.run_id, terminal.revision, RunState::Failed)
            .map_err(|_| CodingAgentShutdownError::Storage)?;

        assert!(matches!(
            self.observed_store
                .transition(&self.run_id, run.revision, RunState::Unknown),
            Err(lookback_tauri_lib::coding_agent::CodingAgentError::Conflict(_))
        ));
        let latest = self
            .observed_store
            .get_run(&self.run_id)
            .map_err(|_| CodingAgentShutdownError::Storage)?
            .ok_or(CodingAgentShutdownError::RunUnavailable)?;
        Ok(CodingAgentShutdownRun {
            run_id: latest.snapshot.run_id.to_string(),
            revision: latest.revision,
            state: latest.state,
        })
    }
}

#[tokio::test]
async fn shutdown_accepts_a_terminal_transition_from_a_second_store_before_unknown_cas() {
    let temp = tempfile::tempdir().unwrap();
    let paths = DataPaths::with_root(temp.path().join("coding-agent-store"));
    let observed_store = RunStore::open(&paths).unwrap();
    let run_id = RunId::new("shutdown-race").unwrap();
    let created = observed_store
        .create_run(shutdown_test_snapshot(run_id.clone()))
        .unwrap();
    let queued = observed_store
        .transition(&run_id, created.revision, RunState::Queued)
        .unwrap();
    observed_store
        .transition(&run_id, queued.revision, RunState::Preparing)
        .unwrap();
    let competing_store = RunStore::open(&paths).unwrap();
    let operations = ConcurrentStoreShutdownOperations {
        observed_store: &observed_store,
        competing_store: &competing_store,
        run_id: run_id.clone(),
    };

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reconcile_coding_agent_shutdown_runs(
            &operations,
            tokio::time::Instant::now() - std::time::Duration::from_millis(1),
        ),
    )
    .await
    .expect("shutdown reconciliation remains bounded")
    .expect("a confirmed terminal race is resolved rather than a storage failure");

    assert!(outcome.unconfirmed_run_ids.is_empty());
    let final_run = observed_store.get_run(&run_id).unwrap().unwrap();
    assert_eq!(final_run.state, RunState::Failed);
}

fn shutdown_test_snapshot(run_id: RunId) -> AgentRunSnapshot {
    let repository = RepositoryIdentity::new("forge.example", "lookback", "agent-app").unwrap();
    let image_digest = format!("sha256:{}", "a".repeat(64));
    AgentRunSnapshot {
        run_id: run_id.clone(),
        issue_id: "issue-shutdown-race".into(),
        issue_revision: 1,
        execution_mode: ExecutionMode::OneShot,
        purpose: RunPurpose::Implementation,
        repository: ResolvedRepository {
            identity: repository.clone(),
            forge: ForgeKind::Gitea,
            api_base_url: "https://forge.example/api/v1".into(),
            clone_url: "https://forge.example/lookback/agent-app".into(),
            read_credential_ref: None,
            publication_credential_ref: None,
            provenance: RepositoryProvenance {
                source_labels: vec!["repo:forge.example/lookback/agent-app".into()],
                selection: RepositorySelection::SourceLabel,
                approved_by: None,
                approved_at_ms: None,
            },
        },
        profile: AgentProfileSnapshot {
            profile_id: "local-coder".into(),
            version: 1,
            display_name: "Local coder".into(),
            image_ref: format!("registry.example/coder@{image_digest}"),
            image_digest,
            command: vec!["agent".into(), "run".into()],
            container_home: "/home/agent".into(),
            managed_home_overlay: None,
            runtime_environment: Default::default(),
            network_access: Default::default(),
            resources: SandboxResources {
                cpu_millis: 1000,
                memory_mib: 1024,
                root_disk_mib: 4096,
                max_parallel_runs: 1,
                use_static: false,
            },
            validated_host_limits: HostResourceLimits {
                cpu_millis: 8000,
                memory_mib: 16384,
                root_disk_mib: 65536,
                max_parallel_runs: 4,
            },
            canonical_mount_allowed_roots: Vec::new(),
            persistent_mounts: Vec::new(),
            allowed_repositories: vec![repository],
            credential_reference: None,
        },
        base_commit: "1".repeat(40),
        workspace_branch: format!("agent/{run_id}"),
        semantic_judge_enabled: true,
        auto_done: false,
        approval: Some(RunApproval {
            approved_by: "reviewer".into(),
            approved_at_ms: 1,
            auto_done_approved: false,
            settings_version: Some(1),
            applicable_modes: vec![ExecutionMode::OneShot],
        }),
        allowed_operations: vec![AllowedOperation::Verify],
        publication_plan: PublicationPlan::NoPublication,
        workflow: WorkflowDefinitionSnapshot {
            definition_hash: "c".repeat(64),
            semantic_judge_worker_digest: None,
            template_id: Some("coding-agent".into()),
            materialized_definition_hash: Some("d".repeat(64)),
            registered_worker_id: Some("41".into()),
            workflow_execution_id: Some("workflow-41".into()),
            parent_job_id: Some("parent-41".into()),
            child_sandbox_job_id: Some("child-41".into()),
        },
        attempt_ids: vec!["attempt-shutdown".into()],
    }
}

#[test]
fn close_dialog_allows_only_one_pending_request_and_cancel_has_no_shutdown_effect() {
    let lifecycle = WindowLifecycle::default();
    let request = lifecycle
        .begin_close_request("main", TrayAvailability::VerifiedRestorable)
        .unwrap();

    assert!(
        lifecycle
            .begin_close_request("main", TrayAvailability::VerifiedRestorable)
            .is_none()
    );
    assert!(lifecycle.acknowledge_close_request(request.request_id));
    assert!(lifecycle.acknowledge_close_request(request.request_id));
    assert_eq!(
        lifecycle.select_close_action(
            request.request_id,
            CloseChoice::Cancel,
            TrayAvailability::VerifiedRestorable,
        ),
        CloseActionResult::Accepted
    );
    assert!(lifecycle.pending_close_request().is_none());
    assert!(!lifecycle.is_quit_in_progress());
}

#[test]
fn ui_reregistration_can_recover_a_request_after_a_webview_reload() {
    let lifecycle = WindowLifecycle::default();
    let request = lifecycle
        .begin_close_request("main", TrayAvailability::VerifiedRestorable)
        .unwrap();
    assert!(lifecycle.acknowledge_close_request(request.request_id));

    assert_eq!(lifecycle.register_ui("en"), Some(request.clone()));
    assert!(lifecycle.acknowledge_close_request(request.request_id));
    assert_eq!(
        lifecycle.select_close_action(
            request.request_id,
            CloseChoice::Cancel,
            TrayAvailability::VerifiedRestorable,
        ),
        CloseActionResult::Accepted
    );
}

#[test]
fn background_action_rechecks_restore_path_and_never_hides_when_unverified() {
    let lifecycle = WindowLifecycle::default();
    let request = lifecycle
        .begin_close_request("main", TrayAvailability::VerifiedRestorable)
        .unwrap();
    assert!(lifecycle.acknowledge_close_request(request.request_id));

    assert_eq!(
        lifecycle.select_close_action(
            request.request_id,
            CloseChoice::Background,
            TrayAvailability::Unverified,
        ),
        CloseActionResult::BackgroundUnavailable
    );
    let updated = lifecycle.pending_close_request().unwrap();
    assert!(!updated.background_available);
    assert_eq!(
        updated.background_unavailable_reason,
        Some(BackgroundUnavailableReason::Unverified)
    );
    assert_eq!(
        lifecycle.select_close_action(
            request.request_id,
            CloseChoice::Background,
            TrayAvailability::VerifiedRestorable,
        ),
        CloseActionResult::BackgroundUnavailable
    );
    assert_eq!(
        lifecycle.select_close_action(
            request.request_id,
            CloseChoice::Cancel,
            TrayAvailability::Unavailable,
        ),
        CloseActionResult::Accepted
    );

    let request = lifecycle
        .begin_close_request("main", TrayAvailability::VerifiedRestorable)
        .unwrap();
    assert!(lifecycle.acknowledge_close_request(request.request_id));
    assert_eq!(
        lifecycle.select_close_action(
            request.request_id,
            CloseChoice::Background,
            TrayAvailability::VerifiedRestorable,
        ),
        CloseActionResult::Accepted
    );
    assert!(lifecycle.complete_background_action(
        request.request_id,
        Err(BackgroundUnavailableReason::HideFailed)
    ));
    let updated = lifecycle.pending_close_request().unwrap();
    assert!(!updated.background_available);
    assert_eq!(
        updated.background_unavailable_reason,
        Some(BackgroundUnavailableReason::HideFailed)
    );
    assert_eq!(
        lifecycle.select_close_action(
            request.request_id,
            CloseChoice::Background,
            TrayAvailability::VerifiedRestorable,
        ),
        CloseActionResult::BackgroundUnavailable
    );
}

#[test]
fn stale_and_duplicate_close_actions_cannot_affect_a_new_request() {
    let lifecycle = WindowLifecycle::default();
    let first = lifecycle
        .begin_close_request("main", TrayAvailability::VerifiedRestorable)
        .unwrap();
    assert!(lifecycle.acknowledge_close_request(first.request_id));
    assert_eq!(
        lifecycle.select_close_action(
            first.request_id,
            CloseChoice::Cancel,
            TrayAvailability::VerifiedRestorable,
        ),
        CloseActionResult::Accepted
    );

    let second = lifecycle
        .begin_close_request("main", TrayAvailability::VerifiedRestorable)
        .unwrap();
    assert_ne!(first.request_id, second.request_id);
    assert!(!lifecycle.acknowledge_close_request(first.request_id));
    assert_eq!(
        lifecycle.select_close_action(
            first.request_id,
            CloseChoice::Quit,
            TrayAvailability::VerifiedRestorable,
        ),
        CloseActionResult::StaleRequest
    );
    assert!(lifecycle.acknowledge_close_request(second.request_id));
    assert_eq!(
        lifecycle.select_close_action(
            second.request_id,
            CloseChoice::Cancel,
            TrayAvailability::VerifiedRestorable,
        ),
        CloseActionResult::Accepted
    );
    assert_eq!(
        lifecycle.select_close_action(
            second.request_id,
            CloseChoice::Cancel,
            TrayAvailability::VerifiedRestorable,
        ),
        CloseActionResult::StaleRequest
    );
}

#[test]
fn ui_registration_syncs_locale_and_timeout_fallback_rejects_late_acknowledgement() {
    let lifecycle = WindowLifecycle::default();
    let request = lifecycle
        .begin_close_request("main", TrayAvailability::Unverified)
        .unwrap();

    assert_eq!(
        lifecycle.begin_native_fallback(request.request_id),
        Some(NativeFallbackLocale::Bilingual)
    );
    assert!(lifecycle.resolve_native_fallback(request.request_id, false));

    let request = lifecycle
        .begin_close_request("main", TrayAvailability::Unverified)
        .unwrap();
    assert_eq!(lifecycle.register_ui("en"), Some(request.clone()));
    assert_eq!(
        lifecycle.begin_native_fallback(request.request_id),
        Some(NativeFallbackLocale::English)
    );
    assert!(!lifecycle.acknowledge_close_request(request.request_id));
    assert!(lifecycle.resolve_native_fallback(request.request_id, false));
    assert!(!lifecycle.resolve_native_fallback(request.request_id, true));

    let next = lifecycle
        .begin_close_request("main", TrayAvailability::Unverified)
        .unwrap();
    assert_eq!(lifecycle.register_ui("ja-JP"), Some(next.clone()));
    assert_eq!(
        lifecycle.begin_native_fallback(next.request_id),
        Some(NativeFallbackLocale::Japanese)
    );
}

#[test]
fn ui_reregistration_after_failed_backgrounding_restores_the_disable_reason() {
    let lifecycle = WindowLifecycle::default();
    let request = lifecycle
        .begin_close_request("main", TrayAvailability::VerifiedRestorable)
        .unwrap();
    assert!(lifecycle.acknowledge_close_request(request.request_id));
    assert_eq!(
        lifecycle.select_close_action(
            request.request_id,
            CloseChoice::Background,
            TrayAvailability::VerifiedRestorable,
        ),
        CloseActionResult::Accepted
    );
    assert!(!lifecycle.acknowledge_close_request(request.request_id));
    assert!(lifecycle.complete_background_action(
        request.request_id,
        Err(BackgroundUnavailableReason::HideFailed)
    ));

    let restored = lifecycle.register_ui("ja").unwrap();
    assert!(!restored.background_available);
    assert_eq!(
        restored.background_unavailable_reason,
        Some(BackgroundUnavailableReason::HideFailed)
    );
    assert!(lifecycle.acknowledge_close_request(restored.request_id));
}

#[test]
fn native_fallback_quit_failure_retains_locale_for_a_native_retry_prompt() {
    let lifecycle = WindowLifecycle::default();
    lifecycle.sync_ui_locale("ja");
    let request = lifecycle
        .begin_close_request("main", TrayAvailability::Unverified)
        .unwrap();
    assert_eq!(
        lifecycle.begin_native_fallback(request.request_id),
        Some(NativeFallbackLocale::Japanese)
    );
    assert!(lifecycle.resolve_native_fallback(request.request_id, true));
    assert!(lifecycle.begin_exit_requested());
    assert!(lifecycle.finish_exit(false));

    assert_eq!(
        lifecycle.take_native_fallback_retry_locale(),
        Some(NativeFallbackLocale::Japanese)
    );
    assert!(lifecycle.request_native_fallback_quit(NativeFallbackLocale::Japanese));
    assert!(lifecycle.begin_exit_requested());
    assert!(lifecycle.finish_exit(true));
}

#[test]
fn tray_quit_invalidates_an_open_choice_and_still_uses_one_exit_guard() {
    let lifecycle = WindowLifecycle::default();
    let request = lifecycle
        .begin_close_request("main", TrayAvailability::VerifiedRestorable)
        .unwrap();
    assert!(lifecycle.acknowledge_close_request(request.request_id));

    assert!(lifecycle.begin_exit_requested());
    assert!(lifecycle.pending_close_request().is_none());
    assert_eq!(
        lifecycle.select_close_action(
            request.request_id,
            CloseChoice::Quit,
            TrayAvailability::VerifiedRestorable,
        ),
        CloseActionResult::StaleRequest
    );
    assert!(!lifecycle.begin_exit_requested());
    assert!(lifecycle.finish_exit(true));
}

#[test]
fn failed_quit_releases_the_exit_guard_and_allows_a_fresh_retry() {
    let lifecycle = WindowLifecycle::default();
    let request = lifecycle
        .begin_close_request("main", TrayAvailability::Unverified)
        .unwrap();
    assert!(lifecycle.acknowledge_close_request(request.request_id));
    assert_eq!(
        lifecycle.select_close_action(
            request.request_id,
            CloseChoice::Quit,
            TrayAvailability::Unverified,
        ),
        CloseActionResult::Accepted
    );
    assert!(lifecycle.begin_exit_requested());
    assert!(!lifecycle.begin_exit_requested());
    assert!(lifecycle.finish_exit(false));
    assert!(!lifecycle.is_quit_in_progress());
    assert!(lifecycle.pending_close_request().is_none());
    assert_eq!(
        lifecycle.select_close_action(
            request.request_id,
            CloseChoice::Quit,
            TrayAvailability::Unavailable,
        ),
        CloseActionResult::StaleRequest
    );
    let later = lifecycle
        .begin_close_request("main", TrayAvailability::Unavailable)
        .expect("a failed quit must not leave a stale close request blocking future closes");
    assert_ne!(later.request_id, request.request_id);
    assert!(lifecycle.acknowledge_close_request(later.request_id));
    assert_eq!(
        lifecycle.select_close_action(
            later.request_id,
            CloseChoice::Cancel,
            TrayAvailability::Unavailable,
        ),
        CloseActionResult::Accepted
    );
    assert!(lifecycle.request_quit());
    assert!(lifecycle.begin_exit_requested());
    assert!(lifecycle.finish_exit(true));
}

#[test]
fn exit_failure_restores_a_window_or_selects_a_localized_native_retry() {
    struct FakeRestoreWindow {
        show: bool,
        unminimize: bool,
        focus: bool,
        calls: std::sync::Mutex<Vec<&'static str>>,
    }

    impl WindowRestoreAdapter for FakeRestoreWindow {
        fn show(&self) -> bool {
            self.calls.lock().unwrap().push("show");
            self.show
        }

        fn unminimize(&self) -> bool {
            self.calls.lock().unwrap().push("unminimize");
            self.unminimize
        }

        fn focus(&self) -> bool {
            self.calls.lock().unwrap().push("focus");
            self.focus
        }
    }

    let restored_window = FakeRestoreWindow {
        show: true,
        unminimize: true,
        focus: true,
        calls: Default::default(),
    };
    assert_eq!(
        plan_exit_retry(Some(&restored_window), None, NativeFallbackLocale::English),
        ExitRetryPlan {
            native_dialog: false,
            locale: None,
        }
    );
    assert_eq!(
        *restored_window.calls.lock().unwrap(),
        vec!["show", "unminimize", "focus"]
    );

    let failed_restore = FakeRestoreWindow {
        show: false,
        unminimize: true,
        focus: true,
        calls: Default::default(),
    };
    assert_eq!(
        plan_exit_retry(Some(&failed_restore), None, NativeFallbackLocale::Japanese),
        ExitRetryPlan {
            native_dialog: true,
            locale: Some(NativeFallbackLocale::Japanese),
        }
    );

    assert_eq!(
        plan_exit_retry(None, None, NativeFallbackLocale::English),
        ExitRetryPlan {
            native_dialog: true,
            locale: Some(NativeFallbackLocale::English),
        }
    );

    let native_fallback = FakeRestoreWindow {
        show: true,
        unminimize: true,
        focus: true,
        calls: Default::default(),
    };
    assert_eq!(
        plan_exit_retry(
            Some(&native_fallback),
            Some(NativeFallbackLocale::Japanese),
            NativeFallbackLocale::English,
        ),
        ExitRetryPlan {
            native_dialog: true,
            locale: Some(NativeFallbackLocale::Japanese),
        }
    );
}

struct UnusedTerminalControl;

#[async_trait::async_trait]
impl TerminalControl for UnusedTerminalControl {
    async fn send_input(&self, _bytes: Vec<u8>) -> Result<InputDelivery, ExecutionResult> {
        Err(ExecutionResult::Unsupported)
    }

    async fn finish_input(&self) -> Result<InputDelivery, ExecutionResult> {
        Err(ExecutionResult::Unsupported)
    }
}

#[test]
fn managed_profile_home_is_private_persistent_and_separate_for_each_profile() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settings.json");
    let store = CodingAgentSettingsStore::new(&path);
    let mut initial = empty_settings();
    initial.profiles = vec![profile("first"), profile("second")];
    let initial = store.compare_and_swap(0, initial, &HashSet::new()).unwrap();
    let prepared = store
        .prepare_profile_home(initial.revision, "first", &HashSet::new())
        .unwrap();
    let first = prepared
        .profiles
        .iter()
        .find(|p| p.profile_id == "first")
        .unwrap();
    let mount = &first.persistent_mounts[0];
    assert_eq!(mount.guest_path, first.container_home);
    assert!(!mount.read_only);
    // Agents install and run tools from their home (e.g. the Codex daemon).
    assert!(mount.executable);
    assert!(mount.host_path.is_dir());
    assert_eq!(first.mount_allowed_roots, vec![mount.host_path.clone()]);
    assert_eq!(first.version, initial.profiles[0].version + 1);
    assert_eq!(prepared.revision, initial.revision + 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&mount.host_path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    fs::write(
        mount.host_path.join("agent-state"),
        b"persisted user settings",
    )
    .unwrap();
    let reopened = CodingAgentSettingsStore::new(&path);
    let reused = reopened
        .prepare_profile_home(prepared.revision, "first", &HashSet::new())
        .unwrap();
    assert_eq!(reused, prepared);
    assert_eq!(
        fs::read(mount.host_path.join("agent-state")).unwrap(),
        b"persisted user settings"
    );
    let second = reopened
        .prepare_profile_home(reused.revision, "second", &HashSet::new())
        .unwrap();
    assert_ne!(
        second.profiles[0].persistent_mounts[0].host_path,
        second.profiles[1].persistent_mounts[0].host_path
    );
}

#[test]
fn managed_profile_home_keeps_a_user_choice_to_disable_execution() {
    let temp = tempfile::tempdir().unwrap();
    let store = CodingAgentSettingsStore::new(temp.path().join("settings.json"));
    let mut initial = empty_settings();
    initial.profiles = vec![profile("first")];
    let initial = store.compare_and_swap(0, initial, &HashSet::new()).unwrap();
    let prepared = store
        .prepare_profile_home(initial.revision, "first", &HashSet::new())
        .unwrap();
    let mut restricted = prepared.clone();
    restricted.profiles[0].persistent_mounts[0].executable = false;
    let restricted = store
        .compare_and_swap(prepared.revision, restricted, &HashSet::new())
        .unwrap();

    let again = store
        .prepare_profile_home(restricted.revision, "first", &HashSet::new())
        .unwrap();

    assert_eq!(again, restricted);
    assert!(!again.profiles[0].persistent_mounts[0].executable);
}

#[test]
fn managed_profile_home_refuses_stale_busy_missing_and_conflicting_profiles_without_changes() {
    let temp = tempfile::tempdir().unwrap();
    let store = CodingAgentSettingsStore::new(temp.path().join("settings.json"));
    let mut initial = empty_settings();
    initial.profiles = vec![profile("first")];
    let initial = store.compare_and_swap(0, initial, &HashSet::new()).unwrap();
    assert!(
        store
            .prepare_profile_home(initial.revision - 1, "first", &HashSet::new())
            .is_err()
    );
    assert!(
        store
            .prepare_profile_home(initial.revision, "missing", &HashSet::new())
            .is_err()
    );
    assert!(
        store
            .prepare_profile_home(initial.revision, "first", &HashSet::from(["first".into()]))
            .is_err()
    );
    assert_eq!(store.load().unwrap(), initial);
    assert!(!temp.path().join("profile-homes").exists());
    let shared = temp.path().join("selected-shared-ssh");
    fs::create_dir(&shared).unwrap();
    let mut conflicting = initial.clone();
    conflicting.profiles[0].mount_allowed_roots = vec![shared.clone()];
    conflicting.profiles[0].persistent_mounts = vec![PersistentMountSettings {
        host_path: shared,
        guest_path: "/home/agent/.ssh".into(),
        read_only: true,
        executable: false,
    }];
    let conflicting = store
        .compare_and_swap(initial.revision, conflicting, &HashSet::new())
        .unwrap();
    assert!(
        store
            .prepare_profile_home(conflicting.revision, "first", &HashSet::new())
            .is_err()
    );
    assert_eq!(store.load().unwrap(), conflicting);
    assert!(!temp.path().join("profile-homes").exists());
}

#[cfg(unix)]
#[test]
fn managed_profile_home_rejects_replaced_symlinks_without_writing_outside_its_root() {
    let temp = tempfile::tempdir().unwrap();
    let store = CodingAgentSettingsStore::new(temp.path().join("settings.json"));
    let mut initial = empty_settings();
    initial.profiles = vec![profile("first")];
    let initial = store.compare_and_swap(0, initial, &HashSet::new()).unwrap();
    let prepared = store
        .prepare_profile_home(initial.revision, "first", &HashSet::new())
        .unwrap();
    let home = &prepared.profiles[0].persistent_mounts[0].host_path;
    fs::remove_dir(home).unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, home).unwrap();
    assert!(
        store
            .prepare_profile_home(prepared.revision, "first", &HashSet::new())
            .is_err()
    );
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    assert_eq!(store.load().unwrap(), prepared);
}

#[cfg(unix)]
#[test]
fn managed_profile_home_refuses_unusable_or_shared_permissions_without_chmod() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let store = CodingAgentSettingsStore::new(temp.path().join("settings.json"));
    let mut initial = empty_settings();
    initial.profiles = vec![profile("first")];
    let initial = store.compare_and_swap(0, initial, &HashSet::new()).unwrap();
    let prepared = store
        .prepare_profile_home(initial.revision, "first", &HashSet::new())
        .unwrap();
    let home = &prepared.profiles[0].persistent_mounts[0].host_path;
    for mode in [0o500, 0o400, 0o000, 0o755, 0o770] {
        fs::set_permissions(home, fs::Permissions::from_mode(mode)).unwrap();
        assert!(
            store
                .prepare_profile_home(prepared.revision, "first", &HashSet::new())
                .is_err(),
            "mode {mode:o}"
        );
        assert_eq!(
            fs::metadata(home).unwrap().permissions().mode() & 0o777,
            mode
        );
        assert_eq!(store.load().unwrap(), prepared);
    }
    fs::set_permissions(home, fs::Permissions::from_mode(0o700)).unwrap();
}
