//! Integration tests for sidecar lifecycle.
//!
//! Real spawn-of-the-real-binaries is covered by the manual macOS smoke
//! checklist (AC-1 in tauri-mvp-spec.md). Keeping it out of `cargo test`
//! avoids requiring jobworkerp + memories to be present in CI.
//!
//! This test asserts the lifecycle layer's "passive" contract — the bits
//! that don't require a working gRPC sidecar to be present. The richer
//! flow (start → health → stop) is verified end-to-end on a real machine.

use std::path::PathBuf;

#[cfg(any(debug_assertions, feature = "experimental-issues"))]
use std::sync::Arc;

use lookback_tauri_lib::data::DataPaths;
use lookback_tauri_lib::error::AppError;
use lookback_tauri_lib::sidecar::{SidecarConfig, Sidecars};

fn tmp_root(label: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("lookback-it-{}-{}", label, std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn make_sidecars(tmp: PathBuf) -> Sidecars {
    let sleep = which::which("sleep").expect("`sleep` not found on PATH");
    let data = DataPaths::with_root(tmp);
    data.ensure().unwrap();
    let lance_home = data.lance_language_model_home();
    let config = SidecarConfig {
        jobworkerp_bin: sleep.clone(),
        memories_bin: sleep.clone(),
        memories_db_migrate_bin: PathBuf::from("/bin/true"),
        startup_database_migration_enabled: false,
        conductor_bin: sleep,
        data: data.clone(),
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
    };
    Sidecars::new(config)
}

#[cfg(any(debug_assertions, feature = "experimental-issues"))]
fn staged_binary(dir: &std::path::Path, name: &str) -> PathBuf {
    std::fs::read_dir(dir)
        .expect("read staged sidecar directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.starts_with(&format!("{name}-")))
        })
        .unwrap_or_else(|| panic!("staged {name} binary is missing from {}", dir.display()))
}

#[tokio::test]
#[ignore = "requires staged sidecar binaries; opt in with LOOKBACK_SIDECAR_BIN_DIR"]
#[cfg(any(debug_assertions, feature = "experimental-issues"))]
async fn issue_tracker_toggle_controls_child_status_and_endpoint_environment() {
    let bin_dir = PathBuf::from(
        std::env::var("LOOKBACK_SIDECAR_BIN_DIR").expect("LOOKBACK_SIDECAR_BIN_DIR is required"),
    );
    let root = tmp_root("issue-tracker-full-start");
    let data = DataPaths::with_root(root.clone());
    let lance_home = data.lance_language_model_home();
    let sidecars = Sidecars::new(SidecarConfig {
        jobworkerp_bin: staged_binary(&bin_dir, "all-in-one"),
        memories_bin: staged_binary(&bin_dir, "front"),
        memories_db_migrate_bin: PathBuf::from("/bin/true"),
        startup_database_migration_enabled: false,
        conductor_bin: staged_binary(&bin_dir, "conductor-main"),
        data: data.clone(),
        worker_yaml_paths: Vec::new(),
        issue_tracker_server_bin: Some(staged_binary(&bin_dir, "issue-tracker-server")),
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
    });
    let availability = Arc::new(lookback_tauri_lib::commands::IssueTrackerAvailability::default());
    sidecars.bind_issue_tracker_availability(Arc::clone(&availability));

    let set_tracker_enabled = |enabled| {
        lookback_tauri_lib::commands::issue_tracker_settings::save_issue_tracker_settings(
            &data.issue_tracker_settings_path(),
            &lookback_tauri_lib::commands::issue_tracker_settings::IssueTrackerSettings {
                enabled: Some(enabled),
            },
        )
        .expect("write Issue Tracker setting");
    };
    let endpoint_keys = [
        "ISSUE_TRACKER_GRPC_HOST",
        "ISSUE_TRACKER_GRPC_PORT",
        "ISSUE_TRACKER_GATEWAY_WORKER_HOST",
        "ISSUE_TRACKER_GATEWAY_WORKER_PORT",
    ];

    // Explicit OFF is tested even though this ignored test may run in a debug
    // profile. The setting must override the build default and prevent the
    // child/ports/workflow endpoint from being created.
    set_tracker_enabled(false);
    unsafe {
        for key in endpoint_keys {
            std::env::set_var(key, "stale");
        }
    }
    let off_report = sidecars
        .start()
        .await
        .expect("sidecars start with tracker OFF");
    assert!(
        sidecars
            .current_endpoints()
            .unwrap()
            .issue_tracker_port
            .is_none()
    );
    assert!(sidecars.issue_tracker_url().is_none());
    let off_snapshot = availability.snapshot();
    assert!(!off_snapshot.endpoint_connected);
    assert!(!off_snapshot.gateway_ready);
    assert!(!off_snapshot.mutation_ready);
    assert!(!off_report.warnings.iter().any(|warning| {
        matches!(
            warning.kind,
            lookback_tauri_lib::sidecar::SidecarWarningKind::IssueTrackerUnavailable
        )
    }));
    assert!(endpoint_keys.iter().all(|key| std::env::var(key).is_err()));
    sidecars.stop().await.expect("stop after tracker OFF");

    // OFF → ON must be picked up by the same Sidecars instance after the
    // persisted setting changes; this is the release-build opt-in path.
    set_tracker_enabled(true);
    let on_report = sidecars
        .start()
        .await
        .expect("sidecars start with tracker ON");
    assert!(
        on_report.warnings.iter().all(|warning| {
            !matches!(
                warning.kind,
                lookback_tauri_lib::sidecar::SidecarWarningKind::IssueTrackerUnavailable
            )
        }),
        "Issue Tracker startup warning: {:?}",
        on_report.warnings
    );
    assert!(
        sidecars
            .current_endpoints()
            .unwrap()
            .issue_tracker_port
            .is_some()
    );
    assert!(sidecars.issue_tracker_url().is_some());
    assert!(availability.require_ready().is_ok());
    sidecars.stop().await.expect("stop after tracker ON");

    // ON → OFF must reset the long-lived availability snapshot and clear the
    // inherited endpoint env, not merely hide the UI warning.
    set_tracker_enabled(false);
    unsafe {
        for key in endpoint_keys {
            std::env::set_var(key, "stale");
        }
    }
    let final_off_report = sidecars
        .start()
        .await
        .expect("sidecars restart with tracker OFF");
    assert!(
        sidecars
            .current_endpoints()
            .unwrap()
            .issue_tracker_port
            .is_none()
    );
    assert!(sidecars.issue_tracker_url().is_none());
    let final_off_snapshot = availability.snapshot();
    assert!(!final_off_snapshot.endpoint_connected);
    assert!(!final_off_snapshot.gateway_ready);
    assert!(!final_off_snapshot.mutation_ready);
    assert!(!final_off_report.warnings.iter().any(|warning| {
        matches!(
            warning.kind,
            lookback_tauri_lib::sidecar::SidecarWarningKind::IssueTrackerUnavailable
        )
    }));
    assert!(endpoint_keys.iter().all(|key| std::env::var(key).is_err()));
    sidecars.stop().await.expect("final stop");

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn stop_is_idempotent_before_start() {
    let tmp = tmp_root("stop-before-start");
    let sidecars = make_sidecars(tmp.clone());

    // stop() before start() must not panic, even when nothing was launched.
    sidecars.stop().await.unwrap();
    sidecars.stop().await.unwrap();

    assert!(sidecars.current_endpoints().is_none());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn current_endpoints_is_none_before_start() {
    let tmp = tmp_root("eps-before-start");
    let sidecars = make_sidecars(tmp.clone());

    assert!(sidecars.current_endpoints().is_none());

    let _ = std::fs::remove_dir_all(&tmp);
}

/// `stop()` must clear the crash-recovery PID file even when no children are
/// tracked (the early-return path). This guards the cleanup that runs alongside
/// releasing the instance lock — both happen via `release_instance_lock`, which
/// `stop` calls AFTER stopping any children so the lock outlives the live
/// processes. (The kill-before-release ordering itself is verified on real
/// hardware; here we pin the observable post-condition.)
#[tokio::test]
async fn stop_clears_pid_file_on_early_return() {
    let tmp = tmp_root("stop-clears-pids");
    let data = DataPaths::with_root(tmp.clone());
    data.ensure().unwrap();

    // Pretend a prior run left a PID record behind.
    let pids_path = data.sidecar_pids_path();
    std::fs::write(&pids_path, "999\t/bin/all-in-one\n").unwrap();
    assert!(pids_path.exists());

    let sidecars = make_sidecars(tmp.clone());
    // No start() => no tracked procs => early-return branch.
    sidecars.stop().await.unwrap();

    assert!(
        !pids_path.exists(),
        "stop() must clear the PID file via release_instance_lock"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Hold the per-instance advisory lock the way a first, still-running instance
/// would, then assert a second `start()` refuses to spawn. On macOS `flock`
/// blocks even a second fd in the same process (verified separately), so this
/// faithfully simulates "another instance owns the data root".
#[cfg(unix)]
fn hold_lock(path: &std::path::Path) -> std::fs::File {
    use std::os::unix::io::AsRawFd;
    unsafe extern "C" {
        fn flock(fd: i32, op: i32) -> i32;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    // LOCK_EX | LOCK_NB
    let rc = unsafe { flock(file.as_raw_fd(), 2 | 4) };
    assert_eq!(rc, 0, "test fixture must take the lock first");
    file
}

#[cfg(unix)]
#[tokio::test]
async fn start_refuses_when_another_instance_holds_lock() {
    let tmp = tmp_root("instance-lock");
    let data = DataPaths::with_root(tmp.clone());
    data.ensure().unwrap();

    // Seed a `sidecar.pids` as if a first instance recorded its live children.
    // The refused launch must NOT overwrite this file — otherwise a later
    // launch could mistake the first instance's sidecars for orphans.
    let pids_path = data.sidecar_pids_path();
    std::fs::write(&pids_path, "4242\t/bin/all-in-one\n").unwrap();
    let pids_before = std::fs::read_to_string(&pids_path).unwrap();

    // Simulate the first instance holding the lock for its whole lifetime.
    let _held = hold_lock(&data.sidecar_lock_path());

    let sidecars = make_sidecars(tmp.clone());
    let err = sidecars.start().await.expect_err("must refuse to start");
    assert!(
        matches!(err, AppError::AnotherInstanceRunning),
        "expected AnotherInstanceRunning, got: {err:?}"
    );

    // No sidecars were spawned, and the shared PID file is untouched.
    assert!(sidecars.current_endpoints().is_none());
    assert_eq!(
        std::fs::read_to_string(&pids_path).unwrap(),
        pids_before,
        "refused launch must not overwrite the live instance's sidecar.pids"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
