use std::{fs, sync::Arc};

use lookback_tauri_lib::{
    coding_agent::{HostResourceLimits, settings::CodingAgentSettingsStore},
    commands::IssueTrackerAvailability,
    data::DataPaths,
    native_terminal::registry::NativeRuntimeRegistry,
};
use tokio::sync::Mutex;

#[allow(dead_code)]
mod native_terminal_package {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/native_terminal_package.rs"
    ));
}

use native_terminal_package::{NativeFixture, READ_TOKEN, sidecars, write_private_file};

fn host_limits() -> HostResourceLimits {
    HostResourceLimits {
        cpu_millis: 4_000,
        memory_mib: 8_192,
        root_disk_mib: 32_768,
        max_parallel_runs: 4,
    }
}

#[tokio::test]
async fn registry_construction_keeps_the_optional_feature_blocked_when_native_assets_are_absent() {
    let temp = tempfile::tempdir().unwrap();
    let registry = NativeRuntimeRegistry::new(
        DataPaths::with_root(temp.path().join("app-data")),
        sidecars(temp.path()),
        Arc::new(IssueTrackerAvailability::default()),
        Arc::new(CodingAgentSettingsStore::new(
            temp.path().join("settings.json"),
        )),
        Arc::new(Mutex::new(())),
        host_limits(),
        Some(temp.path().join("installed-resources")),
    );

    let status = registry.status("issue-42").await.unwrap();
    assert!(!status.available);
    assert!(status.profiles.is_empty());
    assert!(
        status
            .blockers
            .iter()
            .any(|blocker| blocker.contains("bundled native executor"))
    );
}

#[tokio::test]
async fn registry_reads_the_fixed_policy_and_reports_malformed_policy_without_leaking_payload() {
    let fixture = NativeFixture::new();
    let valid_registry = NativeRuntimeRegistry::new(
        fixture.data_paths.clone(),
        sidecars(fixture.temp.path()),
        Arc::new(IssueTrackerAvailability::default()),
        Arc::new(CodingAgentSettingsStore::new(
            fixture.temp.path().join("settings.json"),
        )),
        Arc::new(Mutex::new(())),
        host_limits(),
        Some(fixture.app_resource_dir.clone()),
    );
    let valid_status = valid_registry.status("issue-42").await.unwrap();
    assert!(
        !valid_status.available,
        "no managed child is running in the fixture"
    );
    assert!(
        valid_status
            .blockers
            .iter()
            .all(|blocker| !blocker.contains("protected native workspace policy"))
    );

    fs::remove_file(fixture.policy_path()).unwrap();
    let missing_policy_registry = NativeRuntimeRegistry::new(
        fixture.data_paths.clone(),
        sidecars(fixture.temp.path()),
        Arc::new(IssueTrackerAvailability::default()),
        Arc::new(CodingAgentSettingsStore::new(
            fixture.temp.path().join("settings.json"),
        )),
        Arc::new(Mutex::new(())),
        host_limits(),
        Some(fixture.app_resource_dir.clone()),
    );
    let missing_policy_status = missing_policy_registry.status("issue-42").await.unwrap();
    assert!(
        missing_policy_status
            .blockers
            .iter()
            .any(|blocker| blocker.contains("not been explicitly approved")),
        "missing policy should remain GUI-approvable: {:?}",
        missing_policy_status.blockers
    );
    assert!(
        missing_policy_status
            .blockers
            .iter()
            .all(|blocker| !blocker.contains("protected native workspace policy"))
    );

    write_private_file(&fixture.policy_path(), b"{");
    let registry = NativeRuntimeRegistry::new(
        fixture.data_paths.clone(),
        sidecars(fixture.temp.path()),
        Arc::new(IssueTrackerAvailability::default()),
        Arc::new(CodingAgentSettingsStore::new(
            fixture.temp.path().join("settings.json"),
        )),
        Arc::new(Mutex::new(())),
        host_limits(),
        Some(fixture.app_resource_dir.clone()),
    );

    let status = registry.status("issue-42").await.unwrap();
    assert!(!status.available);
    assert!(
        status
            .blockers
            .iter()
            .any(|blocker| { blocker.contains("Repair the protected native workspace policy") }),
        "unexpected status blockers: {:?}",
        status.blockers
    );
    assert!(
        status
            .blockers
            .iter()
            .all(|blocker| !blocker.contains(READ_TOKEN))
    );
    assert!(
        status
            .blockers
            .iter()
            .all(|blocker| !blocker.contains(fixture.data_paths.root.to_string_lossy().as_ref()))
    );
}

#[tokio::test]
async fn automatic_preparation_rejects_invalid_requests_before_runtime_or_repository_access() {
    use lookback_tauri_lib::native_terminal::{
        registry::NativeRuntimeRegistryError, wire::NativeTerminalPrepareRequest,
    };
    let temp = tempfile::tempdir().unwrap();
    let registry = NativeRuntimeRegistry::new(
        DataPaths::with_root(temp.path().join("app-data")),
        sidecars(temp.path()),
        Arc::new(IssueTrackerAvailability::default()),
        Arc::new(CodingAgentSettingsStore::new(
            temp.path().join("settings.json"),
        )),
        Arc::new(Mutex::new(())),
        host_limits(),
        None,
    );
    let valid = || NativeTerminalPrepareRequest {
        issue_id: "issue-42".into(),
        profile_id: "profile-1".into(),
        base_branch: String::new(),
        prompt: None,
    };
    for request in [
        NativeTerminalPrepareRequest {
            issue_id: "".into(),
            ..valid()
        },
        NativeTerminalPrepareRequest {
            profile_id: "".into(),
            ..valid()
        },
        NativeTerminalPrepareRequest {
            base_branch: "main\n".into(),
            ..valid()
        },
        NativeTerminalPrepareRequest {
            prompt: Some("bad\0prompt".into()),
            ..valid()
        },
        NativeTerminalPrepareRequest {
            prompt: Some(
                "x".repeat(lookback_tauri_lib::native_terminal::MAX_NATIVE_PROMPT_BYTES + 1),
            ),
            ..valid()
        },
    ] {
        assert!(
            matches!(
                registry.prepare_automatic(request).await,
                Err(NativeRuntimeRegistryError::InvalidRequest)
            ),
            "invalid input was accepted"
        );
    }
    assert!(matches!(
        registry.prepare_automatic(valid()).await,
        Err(NativeRuntimeRegistryError::Unavailable)
    ));
    assert!(
        !temp.path().join("app-data").exists(),
        "a rejected preparation must not create app records or a workspace"
    );
}
