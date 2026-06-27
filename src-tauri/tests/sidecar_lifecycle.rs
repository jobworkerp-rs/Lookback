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
        conductor_bin: sleep,
        data,
        worker_yaml_paths: Vec::new(),
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
