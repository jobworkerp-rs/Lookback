//! Live capability probes for the app-managed coding-agent boundary.
//!
//! A successful SANDBOX stream or executor CLI probe is not host finalization
//! evidence. This module reports the remaining capability gates explicitly.

use std::{
    fs,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::{process::Command, time::timeout};

use crate::{commands::connection::ConnectionMode, jobworkerp::JobworkerpHandle};

const EXECUTOR_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const BACKEND_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_EXECUTOR_PROBE_BYTES: usize = 64 * 1024;
const REQUIRED_EXECUTOR_PROTOCOL: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentBackendStatus {
    pub enabled: bool,
    pub executor_version: Option<String>,
    pub executor_protocol_version: Option<u32>,
    pub executor_features: Vec<String>,
    pub app_managed_jobworkerp_ready: bool,
    pub blockers: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutorCapabilities {
    protocol_version: u32,
    executor_version: String,
    features: Vec<String>,
}

/// Runtime-owned bridge. The helper is only resolved from the app resource
/// directory; neither renderer settings nor process environment can select an
/// executable or send an implicit remote request.
pub struct CodingAgentBackend {
    executor_path: parking_lot::RwLock<Option<PathBuf>>,
    probe_timeout: Duration,
}

impl Default for CodingAgentBackend {
    fn default() -> Self {
        Self {
            executor_path: parking_lot::RwLock::new(None),
            probe_timeout: BACKEND_PROBE_TIMEOUT,
        }
    }
}

impl CodingAgentBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_bundled_executor_path(&self, path: Option<PathBuf>) {
        *self.executor_path.write() = path;
    }

    #[cfg(test)]
    fn with_probe_timeout(probe_timeout: Duration) -> Self {
        Self {
            executor_path: parking_lot::RwLock::new(None),
            probe_timeout,
        }
    }

    pub const fn probe_timeout() -> Duration {
        BACKEND_PROBE_TIMEOUT
    }

    /// Probe the exact bundled executable and the local sidecar separately.
    /// Missing proof is returned as actionable UI blockers instead of being
    /// converted into a fake runtime observation for the coordinator.
    pub async fn status(
        &self,
        connection_mode: ConnectionMode,
        jobworkerp: Option<&JobworkerpHandle>,
    ) -> CodingAgentBackendStatus {
        let handle = jobworkerp.cloned();
        self.status_with_jobworkerp_future(connection_mode, async move { handle })
            .await
    }

    /// Include local-client acquisition in the same short deadline as both
    /// the helper I/O and the RunnerService RPC.
    pub async fn status_with_jobworkerp_future<F>(
        &self,
        connection_mode: ConnectionMode,
        jobworkerp: F,
    ) -> CodingAgentBackendStatus
    where
        F: std::future::Future<Output = Option<JobworkerpHandle>>,
    {
        match timeout(
            self.probe_timeout,
            self.probe_status(connection_mode, jobworkerp),
        )
        .await
        {
            Ok(status) => status,
            Err(_) => Self::probe_timeout_status(),
        }
    }

    fn probe_timeout_status() -> CodingAgentBackendStatus {
        CodingAgentBackendStatus {
            enabled: false,
            executor_version: None,
            executor_protocol_version: None,
            executor_features: Vec::new(),
            app_managed_jobworkerp_ready: false,
            blockers: vec!["coding_agent_backend_probe_timeout".into()],
        }
    }

    async fn probe_status<F>(
        &self,
        connection_mode: ConnectionMode,
        jobworkerp: F,
    ) -> CodingAgentBackendStatus
    where
        F: std::future::Future<Output = Option<JobworkerpHandle>>,
    {
        let jobworkerp = jobworkerp.await;
        let mut blockers = Vec::new();
        let mut executor_version = None;
        let mut executor_protocol_version = None;
        let mut executor_features = Vec::new();
        match self.probe_executor().await {
            Ok(capabilities) => {
                executor_version = Some(capabilities.executor_version);
                executor_protocol_version = Some(capabilities.protocol_version);
                executor_features = capabilities.features;
            }
            Err(reason) => blockers.push(reason),
        }

        let app_managed_jobworkerp_ready = if connection_mode != ConnectionMode::Local {
            blockers.push("coding_agent_requires_app_managed_local_jobworkerp".into());
            false
        } else if let Some(handle) = jobworkerp.as_ref() {
            match handle.probe_runner_service().await {
                Ok(()) => true,
                Err(_) => {
                    blockers.push("app_managed_jobworkerp_unavailable".into());
                    false
                }
            }
        } else {
            blockers.push("app_managed_jobworkerp_unavailable".into());
            false
        };

        for required in [
            "native_executor_configuration",
            "verified_execution_adapter",
            "registered_coding_agent_workflow",
        ] {
            blockers.push(format!("{required}_unavailable"));
        }
        blockers.sort();
        blockers.dedup();
        CodingAgentBackendStatus {
            enabled: false,
            executor_version,
            executor_protocol_version,
            executor_features,
            app_managed_jobworkerp_ready,
            blockers,
        }
    }

    async fn probe_executor(&self) -> Result<ExecutorCapabilities, String> {
        let path = self
            .executor_path
            .read()
            .clone()
            .ok_or_else(|| "coding_agent_executor_not_bundled".to_owned())?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| "coding_agent_executor_not_bundled".to_owned())?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("coding_agent_executor_artifact_invalid".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o022 != 0
                || metadata.permissions().mode() & 0o111 == 0
            {
                return Err("coding_agent_executor_artifact_invalid".into());
            }
        }
        let mut command = Command::new(path);
        command
            .arg("--capabilities")
            .env_clear()
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        isolate_helper_process_group(&mut command);
        let mut child = command
            .spawn()
            .map_err(|_| "coding_agent_executor_probe_failed".to_owned())?;
        let mut process_group = HelperProcessGroup::new(child.id());
        let collected = timeout(self.probe_timeout.min(EXECUTOR_PROBE_TIMEOUT), async {
            collect_helper_output(&mut child).await
        })
        .await;
        let (status, stdout) = match collected {
            Ok(Ok(output)) => {
                process_group.mark_finished();
                output
            }
            Ok(Err(reason)) => {
                process_group.kill();
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(reason);
            }
            Err(_) => {
                process_group.kill();
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err("coding_agent_executor_probe_timeout".into());
            }
        };
        if !status.success() {
            return Err("coding_agent_executor_probe_failed".into());
        }
        let capabilities: ExecutorCapabilities = serde_json::from_slice(&stdout)
            .map_err(|_| "coding_agent_executor_protocol_invalid".to_owned())?;
        if capabilities.protocol_version != REQUIRED_EXECUTOR_PROTOCOL
            || capabilities.executor_version.trim().is_empty()
        {
            return Err("coding_agent_executor_protocol_incompatible".into());
        }
        Ok(capabilities)
    }
}

async fn collect_helper_output(
    child: &mut tokio::process::Child,
) -> Result<(ExitStatus, Vec<u8>), String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "coding_agent_executor_probe_failed".to_owned())?;
    let mut bytes = Vec::with_capacity(MAX_EXECUTOR_PROBE_BYTES);
    stdout
        .take((MAX_EXECUTOR_PROBE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "coding_agent_executor_probe_failed".to_owned())?;
    if bytes.len() > MAX_EXECUTOR_PROBE_BYTES {
        return Err("coding_agent_executor_output_too_large".into());
    }
    let status = child
        .wait()
        .await
        .map_err(|_| "coding_agent_executor_probe_failed".to_owned())?;
    Ok((status, bytes))
}

fn isolate_helper_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

struct HelperProcessGroup {
    #[cfg(unix)]
    pid: Option<u32>,
    finished: bool,
}

impl HelperProcessGroup {
    fn new(pid: Option<u32>) -> Self {
        #[cfg(not(unix))]
        let _ = pid;
        Self {
            #[cfg(unix)]
            pid,
            finished: false,
        }
    }

    fn mark_finished(&mut self) {
        self.finished = true;
    }

    fn kill(&mut self) {
        if self.finished {
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) {
            // The helper is isolated so timeout and overflow cleanup also
            // terminate descendants that inherited its process group.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        self.finished = true;
    }
}

impl Drop for HelperProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{pin::Pin, sync::Arc};

    use futures::Stream;
    use jobworkerp_client::jobworkerp::{
        data::Runner,
        service::{
            CountCondition, CountResponse, CreateRunnerRequest, CreateRunnerResponse,
            FindListRequest, FindRunnerListRequest, OptionalRunnerResponse, RunnerNameRequest,
            SuccessResponse,
            runner_service_server::{RunnerService, RunnerServiceServer},
        },
    };
    use tokio::sync::oneshot;
    use tonic::{Request, Response, Status};

    #[derive(Debug)]
    struct NeverRespondingRunnerService;

    #[tonic::async_trait]
    impl RunnerService for NeverRespondingRunnerService {
        type FindListStream = Pin<Box<dyn Stream<Item = Result<Runner, Status>> + Send>>;
        type FindListByStream = Pin<Box<dyn Stream<Item = Result<Runner, Status>> + Send>>;

        async fn create(
            &self,
            _request: Request<CreateRunnerRequest>,
        ) -> Result<Response<CreateRunnerResponse>, Status> {
            Err(Status::unimplemented("not used"))
        }

        async fn delete(
            &self,
            _request: Request<jobworkerp_client::jobworkerp::data::RunnerId>,
        ) -> Result<Response<SuccessResponse>, Status> {
            Err(Status::unimplemented("not used"))
        }

        async fn find(
            &self,
            _request: Request<jobworkerp_client::jobworkerp::data::RunnerId>,
        ) -> Result<Response<OptionalRunnerResponse>, Status> {
            Err(Status::unimplemented("not used"))
        }

        async fn find_by_name(
            &self,
            _request: Request<RunnerNameRequest>,
        ) -> Result<Response<OptionalRunnerResponse>, Status> {
            std::future::pending().await
        }

        async fn find_list(
            &self,
            _request: Request<FindListRequest>,
        ) -> Result<Response<Self::FindListStream>, Status> {
            Err(Status::unimplemented("not used"))
        }

        async fn count(
            &self,
            _request: Request<CountCondition>,
        ) -> Result<Response<CountResponse>, Status> {
            Err(Status::unimplemented("not used"))
        }

        async fn find_list_by(
            &self,
            _request: Request<FindRunnerListRequest>,
        ) -> Result<Response<Self::FindListByStream>, Status> {
            Err(Status::unimplemented("not used"))
        }

        async fn count_by(
            &self,
            _request: Request<jobworkerp_client::jobworkerp::service::CountRunnerRequest>,
        ) -> Result<Response<CountResponse>, Status> {
            Err(Status::unimplemented("not used"))
        }
    }

    #[tokio::test]
    async fn absent_helper_and_local_runtime_never_report_execution_enabled() {
        let backend = CodingAgentBackend::new();
        let status = backend.status(ConnectionMode::Local, None).await;

        assert!(!status.enabled);
        assert!(
            status
                .blockers
                .contains(&"coding_agent_executor_not_bundled".into())
        );
        assert!(
            !status
                .blockers
                .contains(&"managed_quota_authority_unavailable".into())
        );
        assert!(
            status
                .blockers
                .contains(&"native_executor_configuration_unavailable".into())
        );
        assert!(
            status
                .blockers
                .contains(&"verified_execution_adapter_unavailable".into())
        );
        assert!(
            status
                .blockers
                .contains(&"registered_coding_agent_workflow_unavailable".into())
        );
        assert!(!status.blockers.iter().any(|blocker| {
            [
                "host_process_observation_unavailable",
                "host_writer_quiescence_unavailable",
                "host_workspace_freeze_unavailable",
            ]
            .contains(&blocker.as_str())
        }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn protocol_v2_resolve_base_probe_is_structural_and_keeps_execution_disabled() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("helper");
        std::fs::write(
            &helper,
            br##"#!/bin/sh
printf '%s\n' '{"protocol_version":2,"executor_version":"0.1.0","features":["resolve_base","prepare","capture","seal","resolve","artifact_only_publication"]}'
"##,
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let backend = CodingAgentBackend::with_probe_timeout(Duration::from_secs(1));
        backend.set_bundled_executor_path(Some(helper));

        let status = backend.status(ConnectionMode::Local, None).await;

        assert_eq!(status.executor_protocol_version, Some(2));
        assert_eq!(status.executor_version.as_deref(), Some("0.1.0"));
        assert!(
            status
                .executor_features
                .iter()
                .any(|feature| feature == "resolve_base")
        );
        assert!(!status.enabled);
        assert!(!status.app_managed_jobworkerp_ready);
        assert!(
            status
                .blockers
                .contains(&"app_managed_jobworkerp_unavailable".into())
        );
        assert!(
            status
                .blockers
                .contains(&"native_executor_configuration_unavailable".into())
        );
        assert!(
            status
                .blockers
                .contains(&"verified_execution_adapter_unavailable".into())
        );
        assert!(
            status
                .blockers
                .contains(&"registered_coding_agent_workflow_unavailable".into())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn protocol_v1_executor_probe_is_reported_incompatible() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("helper");
        std::fs::write(
            &helper,
            br##"#!/bin/sh
printf '%s\n' '{"protocol_version":1,"executor_version":"0.1.0","features":["prepare","capture","seal","resolve","artifact_only_publication"]}'
"##,
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let backend = CodingAgentBackend::with_probe_timeout(Duration::from_secs(1));
        backend.set_bundled_executor_path(Some(helper));

        let status = backend.status(ConnectionMode::Local, None).await;

        assert!(!status.enabled);
        assert_eq!(status.executor_protocol_version, None);
        assert!(
            status
                .blockers
                .contains(&"coding_agent_executor_protocol_incompatible".into())
        );
    }

    #[tokio::test]
    async fn remote_jobworkerp_is_not_accepted_as_the_app_managed_executor() {
        let backend = CodingAgentBackend::new();
        let status = backend.status(ConnectionMode::Remote, None).await;

        assert!(!status.app_managed_jobworkerp_ready);
        assert!(
            status
                .blockers
                .contains(&"coding_agent_requires_app_managed_local_jobworkerp".into())
        );
    }

    #[tokio::test]
    async fn full_backend_status_deadline_covers_a_stalled_runner_rpc() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock Runner service");
        let address = listener.local_addr().expect("read mock Runner address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let incoming = futures::stream::try_unfold(listener, |listener| async move {
                listener
                    .accept()
                    .await
                    .map(|(stream, _)| Some((stream, listener)))
            });
            tonic::transport::Server::builder()
                .add_service(RunnerServiceServer::from_arc(Arc::new(
                    NeverRespondingRunnerService,
                )))
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("run mock Runner service");
        });
        let endpoint = format!("http://{address}");
        let handle = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match JobworkerpHandle::connect_with_timeout(&endpoint, 10_800).await {
                    Ok(handle) => break handle,
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .expect("connect to the local mock Runner service");
        let backend = CodingAgentBackend::with_probe_timeout(Duration::from_millis(100));
        let started = tokio::time::Instant::now();

        let status = backend
            .status_with_jobworkerp_future(ConnectionMode::Local, async { Some(handle) })
            .await;

        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            status.blockers,
            vec!["coding_agent_backend_probe_timeout".to_owned()]
        );
        assert!(!status.enabled);
        let _ = shutdown_tx.send(());
        server.await.expect("join mock Runner service");
    }

    #[tokio::test]
    async fn full_backend_status_deadline_includes_local_client_acquisition() {
        let backend = CodingAgentBackend::with_probe_timeout(Duration::from_millis(25));
        let started = tokio::time::Instant::now();

        let status = backend
            .status_with_jobworkerp_future(ConnectionMode::Local, async {
                std::future::pending::<Option<JobworkerpHandle>>().await
            })
            .await;

        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            status.blockers,
            vec!["coding_agent_backend_probe_timeout".to_owned()]
        );
        assert!(!status.enabled);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_output_overflow_is_bounded_and_returns_a_fixed_diagnostic() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("helper");
        std::fs::write(&helper, b"#!/bin/sh\nprintf '%070000d' 0\n").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let backend = CodingAgentBackend::with_probe_timeout(Duration::from_secs(1));
        backend.set_bundled_executor_path(Some(helper));

        let error = backend.probe_executor().await.unwrap_err();

        assert_eq!(error, "coding_agent_executor_output_too_large");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_timeout_kills_its_process_group() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("helper");
        let descendant_marker = temp.path().join("descendant-survived");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\n(/bin/sleep 0.25; /usr/bin/touch '{}') &\nwait\n",
                descendant_marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let backend = CodingAgentBackend::with_probe_timeout(Duration::from_millis(80));
        backend.set_bundled_executor_path(Some(helper));
        let started = tokio::time::Instant::now();

        let error = backend.probe_executor().await.unwrap_err();

        assert_eq!(error, "coding_agent_executor_probe_timeout");
        assert!(started.elapsed() < Duration::from_secs(1));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!descendant_marker.exists());
    }
}
