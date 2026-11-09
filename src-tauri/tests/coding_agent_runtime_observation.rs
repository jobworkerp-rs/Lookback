use std::{path::PathBuf, time::Duration};

use jobworkerp_client::jobworkerp::data::JobId;
use lookback_tauri_lib::{
    coding_agent::runtime_observation::{
        MICROSANDBOX_SDK_VERSION, MicrosandboxObservationConfig, RuntimeArtifactDigests,
        RuntimeObservationError, WorkerIdentity, WorkerRuntimeContext, observe_sandbox_stop,
    },
    jobworkerp::coding_agent::CodingAgentEvent,
};
use microsandbox::config::{GlobalConfig, PathsConfig};

fn valid_runtime_context(
    config: &MicrosandboxObservationConfig,
) -> (WorkerIdentity, WorkerRuntimeContext) {
    let worker = WorkerIdentity::new(17, "lookback-coding-agent", "runtime-instance-3");
    let artifacts = RuntimeArtifactDigests::new(
        "a".repeat(64),
        "b".repeat(64),
        "c".repeat(64),
        "d".repeat(64),
    );
    let runtime = WorkerRuntimeContext {
        worker_id: 17,
        worker_name: "lookback-coding-agent".into(),
        runner_name: "SANDBOX".into(),
        is_static_worker: Some(false),
        worker_snapshot_id: Some("worker-snapshot-17".into()),
        runtime_instance_id: Some("runtime-instance-3".into()),
        microsandbox_sdk_version: Some(MICROSANDBOX_SDK_VERSION.into()),
        child_msb_home: Some(config.home().to_path_buf()),
        child_msb_config_path: Some(config.config_path().to_path_buf()),
        child_msb_sandboxes_dir: Some(config.home().join("sandboxes")),
        child_msb_run_dir: Some(config.home().join("run")),
        pinned_artifacts: Some(artifacts.clone()),
        observed_artifacts: Some(artifacts),
    };
    (worker, runtime)
}

fn observation_config(directory: &std::path::Path) -> MicrosandboxObservationConfig {
    MicrosandboxObservationConfig::new(
        directory.join("child-msb-home"),
        directory.join("child-msb-config.json"),
    )
    .with_timeout(Duration::from_secs(10))
}

fn sandbox_exit() -> CodingAgentEvent {
    CodingAgentEvent::Exit {
        exit_code: 0,
        execution_time_ms: 12,
        sandbox_id: "local:42".into(),
    }
}

#[tokio::test]
async fn unavailable_native_exit_observation_does_not_open_or_destroy_catalog_entries() {
    let directory = tempfile::tempdir().unwrap();
    let config = observation_config(directory.path());
    std::fs::create_dir_all(config.home().join("sandboxes")).unwrap();
    std::fs::create_dir_all(config.home().join("run")).unwrap();
    let (worker, runtime) = valid_runtime_context(&config);

    let result = observe_sandbox_stop(
        &config,
        &worker,
        &runtime,
        &JobId { value: 4242 },
        &sandbox_exit(),
    )
    .await;

    assert_eq!(
        result,
        Err(RuntimeObservationError::UnsupportedPhysicalExitObservation)
    );
    assert!(!config.home().join("db").exists());
}

#[test]
fn effective_sdk_paths_reject_managed_home_or_catalog_redirects_without_io() {
    let directory = tempfile::tempdir().unwrap();
    let config = observation_config(directory.path());
    let child_home = config.home();
    let child_sandboxes = child_home.join("sandboxes");
    let child_run = child_home.join("run");
    std::fs::create_dir_all(&child_sandboxes).unwrap();
    std::fs::create_dir_all(&child_run).unwrap();
    let (worker, runtime) = valid_runtime_context(&config);

    let resolved_child_paths = GlobalConfig {
        home: Some(child_home.to_path_buf()),
        paths: PathsConfig {
            sandboxes: Some(child_sandboxes.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        lookback_tauri_lib::coding_agent::runtime_observation::validate_effective_paths(
            &config,
            &worker,
            &runtime,
            &resolved_child_paths,
        ),
        Ok(())
    );

    let alternate_home = directory.path().join("managed-home");
    std::fs::create_dir_all(alternate_home.join("sandboxes")).unwrap();
    std::fs::create_dir_all(alternate_home.join("run")).unwrap();
    let redirected_home = GlobalConfig {
        home: Some(alternate_home),
        ..Default::default()
    };
    assert_eq!(
        lookback_tauri_lib::coding_agent::runtime_observation::validate_effective_paths(
            &config,
            &worker,
            &runtime,
            &redirected_home,
        ),
        Err(RuntimeObservationError::RuntimePathsMismatch)
    );

    let alternate_sandboxes = directory.path().join("managed-sandboxes");
    std::fs::create_dir_all(&alternate_sandboxes).unwrap();
    let redirected_catalog = GlobalConfig {
        home: Some(child_home.to_path_buf()),
        paths: PathsConfig {
            sandboxes: Some(alternate_sandboxes),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        lookback_tauri_lib::coding_agent::runtime_observation::validate_effective_paths(
            &config,
            &worker,
            &runtime,
            &redirected_catalog,
        ),
        Err(RuntimeObservationError::RuntimePathsMismatch)
    );
}

#[tokio::test]
async fn static_worker_is_rejected_before_opening_the_explicit_catalog() {
    let directory = tempfile::tempdir().unwrap();
    let config = observation_config(directory.path());
    let (worker, mut runtime) = valid_runtime_context(&config);
    runtime.is_static_worker = Some(true);

    let result = observe_sandbox_stop(
        &config,
        &worker,
        &runtime,
        &JobId { value: 4242 },
        &sandbox_exit(),
    )
    .await;

    assert_eq!(result, Err(RuntimeObservationError::StaticWorker));
    assert!(!config.home().exists());
}

#[tokio::test]
async fn missing_worker_staticity_or_matching_runtime_claims_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let config = observation_config(directory.path());
    let (worker, mut runtime) = valid_runtime_context(&config);
    runtime.is_static_worker = None;

    let result = observe_sandbox_stop(
        &config,
        &worker,
        &runtime,
        &JobId { value: 4242 },
        &sandbox_exit(),
    )
    .await;
    assert_eq!(result, Err(RuntimeObservationError::UntrustedContext));

    runtime.is_static_worker = Some(false);
    runtime.observed_artifacts.as_mut().unwrap().agentd = "e".repeat(64);
    let result = observe_sandbox_stop(
        &config,
        &worker,
        &runtime,
        &JobId { value: 4242 },
        &sandbox_exit(),
    )
    .await;
    assert_eq!(result, Err(RuntimeObservationError::UntrustedContext));
    assert!(!config.home().exists());
}

#[tokio::test]
async fn worker_binding_and_typed_exit_are_required() {
    let directory = tempfile::tempdir().unwrap();
    let config = observation_config(directory.path());
    let (worker, runtime) = valid_runtime_context(&config);

    let wrong_worker = WorkerIdentity::new(18, "lookback-coding-agent", "runtime-instance-3");
    let result = observe_sandbox_stop(
        &config,
        &wrong_worker,
        &runtime,
        &JobId { value: 4242 },
        &sandbox_exit(),
    )
    .await;
    assert_eq!(result, Err(RuntimeObservationError::WorkerMismatch));

    let wrong_runtime = WorkerIdentity::new(17, "lookback-coding-agent", "runtime-instance-old");
    let result = observe_sandbox_stop(
        &config,
        &wrong_runtime,
        &runtime,
        &JobId { value: 4242 },
        &sandbox_exit(),
    )
    .await;
    assert_eq!(result, Err(RuntimeObservationError::WorkerMismatch));

    let output = CodingAgentEvent::Output {
        stream: "stdout".into(),
        data: b"not an exit".to_vec(),
    };
    let result =
        observe_sandbox_stop(&config, &worker, &runtime, &JobId { value: 4242 }, &output).await;
    assert_eq!(result, Err(RuntimeObservationError::TypedExitRequired));
    assert!(!config.home().exists());
}

#[tokio::test]
async fn child_home_config_and_canonical_sandbox_identity_must_match() {
    let directory = tempfile::tempdir().unwrap();
    let config = observation_config(directory.path());
    std::fs::create_dir_all(config.home().join("sandboxes")).unwrap();
    std::fs::create_dir_all(config.home().join("run")).unwrap();
    let (worker, mut runtime) = valid_runtime_context(&config);
    runtime.child_msb_home = Some(PathBuf::from("/another/msb-home"));

    let result = observe_sandbox_stop(
        &config,
        &worker,
        &runtime,
        &JobId { value: 4242 },
        &sandbox_exit(),
    )
    .await;
    assert_eq!(result, Err(RuntimeObservationError::RuntimePathsMismatch));

    runtime.child_msb_home = Some(config.home().to_path_buf());
    runtime.child_msb_config_path = Some(PathBuf::from("/another/msb-config.json"));
    let result = observe_sandbox_stop(
        &config,
        &worker,
        &runtime,
        &JobId { value: 4242 },
        &sandbox_exit(),
    )
    .await;
    assert_eq!(result, Err(RuntimeObservationError::RuntimePathsMismatch));

    runtime.child_msb_config_path = Some(config.config_path().to_path_buf());
    let malformed_exit = CodingAgentEvent::Exit {
        exit_code: 0,
        execution_time_ms: 12,
        sandbox_id: "sandbox-name-is-not-a-stable-local-id".into(),
    };
    let result = observe_sandbox_stop(
        &config,
        &worker,
        &runtime,
        &JobId { value: 4242 },
        &malformed_exit,
    )
    .await;
    assert_eq!(result, Err(RuntimeObservationError::InvalidSandboxIdentity));
    assert!(!config.home().join("db").exists());
}
