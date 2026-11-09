use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::Utc;
use parking_lot::Mutex;
use tokio::process::Child;
use tokio::time::timeout;

use super::{
    AppResult, Process, SidecarWarning, SidecarWarningKind, Sidecars, forward_output, spawn_err,
    wait_for_tcp_or_failure,
};

pub(super) fn configured_work_eligibility(
    data_root: &Path,
) -> crate::grpc::proto::issue_tracker::manager::v1::WorkEligibilityMode {
    crate::issue_work_policy::WorkEligibilityRegistry::open(data_root)
        .load()
        .gateway_mode()
}

pub(super) const ISSUE_TRACKER_ENDPOINT_ENV_KEYS: [&str; 6] = [
    "ISSUE_TRACKER_GRPC_HOST",
    "ISSUE_TRACKER_GRPC_PORT",
    "ISSUE_TRACKER_GRPC_TLS",
    "ISSUE_TRACKER_GATEWAY_WORKER_HOST",
    "ISSUE_TRACKER_GATEWAY_WORKER_PORT",
    "ISSUE_TRACKER_GATEWAY_WORKER_TLS",
];

const ISSUE_TRACKER_MAINTENANCE_INTERVAL_ENV_KEY: &str =
    "LOOKBACK_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS";
const DEFAULT_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS: u64 = 5;
const MIN_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS: u64 = 1;

fn resolve_issue_tracker_maintenance_interval(raw: Option<&str>) -> Duration {
    let Some(raw) = raw else {
        return Duration::from_secs(DEFAULT_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS);
    };

    match raw.trim().parse::<u64>() {
        Ok(seconds) => {
            Duration::from_secs(seconds.max(MIN_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS))
        }
        Err(error) => {
            tracing::warn!(
                env_key = ISSUE_TRACKER_MAINTENANCE_INTERVAL_ENV_KEY,
                value = raw,
                error = %error,
                default_secs = DEFAULT_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS,
                "invalid Issue Tracker maintenance interval; using default"
            );
            Duration::from_secs(DEFAULT_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IssueTrackerStartDecision {
    Disabled,
    Start,
    Unavailable,
}

pub(super) fn issue_tracker_start_decision(
    enabled: bool,
    binary_configured: bool,
) -> IssueTrackerStartDecision {
    if !enabled {
        IssueTrackerStartDecision::Disabled
    } else if binary_configured {
        IssueTrackerStartDecision::Start
    } else {
        IssueTrackerStartDecision::Unavailable
    }
}

/// Publish the remote tracker in `connection` as jobworkerp reaches it.
pub(super) fn publish_remote_issue_tracker_endpoint_env(
    connection: &crate::commands::connection::ConnectionConfig,
) -> AppResult<()> {
    let (primary, gateway_worker) =
        crate::commands::connection::remote_issue_tracker_for_jobworkerp(connection)?;
    publish_issue_tracker_endpoint_env(&primary, &gateway_worker);
    Ok(())
}

/// Sets the tracker endpoint, as jobworkerp reaches it, in this process's
/// environment, where sidecars spawned afterwards, worker YAML expansion and
/// the app's workflow inputs read it.
pub(super) fn publish_issue_tracker_endpoint_env(
    primary: &crate::commands::connection::GrpcAddress,
    gateway_worker: &crate::commands::connection::GrpcAddress,
) {
    unsafe {
        for (key, value) in issue_tracker_endpoint_env_values(primary, gateway_worker) {
            std::env::set_var(key, value);
        }
    }
}

fn issue_tracker_endpoint_env_values(
    primary: &crate::commands::connection::GrpcAddress,
    gateway_worker: &crate::commands::connection::GrpcAddress,
) -> [(&'static str, String); 6] {
    let [host, port, tls, gateway_host, gateway_port, gateway_tls] =
        ISSUE_TRACKER_ENDPOINT_ENV_KEYS;
    [
        (host, primary.host.clone()),
        (port, primary.port.to_string()),
        (tls, primary.tls.to_string()),
        (gateway_host, gateway_worker.host.clone()),
        (gateway_port, gateway_worker.port.to_string()),
        (gateway_tls, gateway_worker.tls.to_string()),
    ]
}

/// The tracker as jobworkerp reaches it, as last published; `None` while
/// Issue tracking is off. Workflow inputs carry it because a remote
/// jobworkerp has no app-set environment to fall back on.
pub(crate) fn published_issue_tracker_for_jobworkerp()
-> Option<crate::commands::connection::GrpcAddress> {
    issue_tracker_from_env(|key| std::env::var(key).ok())
}

fn issue_tracker_from_env(
    lookup: impl Fn(&str) -> Option<String>,
) -> Option<crate::commands::connection::GrpcAddress> {
    let [host, port, tls, ..] = ISSUE_TRACKER_ENDPOINT_ENV_KEYS.map(lookup);
    Some(crate::commands::connection::GrpcAddress {
        host: host.filter(|host| !host.is_empty())?,
        port: port?.parse().ok()?,
        tls: tls.is_some_and(|tls| tls == "true"),
    })
}

pub(super) fn clear_issue_tracker_endpoint_env() {
    // The sidecar command inherits the parent environment, so disabling the
    // tracker must remove endpoints that an earlier enabled launch installed.
    unsafe {
        for key in ISSUE_TRACKER_ENDPOINT_ENV_KEYS {
            std::env::remove_var(key);
        }
    }
}

async fn ensure_gateway_policy(
    service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
    gateway_control_endpoint: &str,
    data_root: &Path,
    correlation_id: &str,
) -> AppResult<()> {
    let expected = configured_work_eligibility(data_root);
    if let Ok(registration) = service
        .get_gateway_registration(gateway_control_endpoint, correlation_id)
        .await
        && crate::grpc::issue_tracker::gateway_registration_matches(&registration, expected)
    {
        return Ok(());
    }

    let idempotency_key = format!(
        "lookback-local-gateway-register-{}-{expected:?}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    service
        .register_and_verify_gateway_policy(
            gateway_control_endpoint,
            correlation_id,
            &idempotency_key,
            expected,
        )
        .await
        .map(|_| ())
}

/// Extra worker YAMLs applied after the configured list.
///
/// Gateway workers stay registered even when the tracker is unavailable. This
/// keeps the MCP tool surface stable for clients that cache `tools/list`; a
/// call then fails against the reserved loopback endpoint until the tracker
/// becomes healthy.
pub(super) fn extra_issue_tracker_worker_yaml_paths(enabled: bool) -> Vec<PathBuf> {
    if !enabled {
        return Vec::new();
    }
    let mut paths = Vec::new();
    if let Ok(path) = crate::data::paths::lookback_issue_extract_workers_yaml() {
        paths.push(path);
    }
    if let Ok(path) = crate::data::paths::lookback_issue_gateway_workers_yaml() {
        paths.push(path);
    }
    paths
}

pub(super) fn mark_issue_tracker_maintenance_unavailable(
    availability: Option<&Arc<crate::commands::IssueTrackerAvailability>>,
    _issue_tracker_port: &Mutex<Option<u16>>,
    reason: impl Into<String>,
) {
    if let Some(availability) = availability {
        availability.mark_mutation_unavailable(reason.into());
    }
}

pub(super) fn mark_issue_tracker_gateway_unavailable(
    availability: Option<&Arc<crate::commands::IssueTrackerAvailability>>,
    reason: impl Into<String>,
) {
    if let Some(availability) = availability {
        availability.mark_gateway_unavailable(reason.into());
    }
}

pub(super) fn mark_issue_tracker_maintenance_ready(
    availability: Option<&Arc<crate::commands::IssueTrackerAvailability>>,
    issue_tracker_port: &Mutex<Option<u16>>,
    port: u16,
) {
    mark_issue_tracker_mutation_ready(availability, issue_tracker_port, port);
    if let Some(availability) = availability {
        availability.mark_gateway_ready();
    }
}

pub(super) fn mark_issue_tracker_mutation_ready(
    availability: Option<&Arc<crate::commands::IssueTrackerAvailability>>,
    issue_tracker_port: &Mutex<Option<u16>>,
    port: u16,
) {
    *issue_tracker_port.lock() = Some(port);
    if let Some(availability) = availability {
        availability.mark_mutation_ready();
    }
}

impl Sidecars {
    /// Bind the command-layer Issue tracker availability state to this
    /// lifecycle. Rebinding is harmless and keeps test setup lightweight.
    pub fn bind_issue_tracker_availability(
        &self,
        availability: Arc<crate::commands::IssueTrackerAvailability>,
    ) {
        *self.issue_tracker_availability.lock() = Some(availability);
    }

    pub(super) fn mark_issue_tracker_ready(&self) {
        if let Some(availability) = self.issue_tracker_availability.lock().as_ref() {
            availability.mark_gateway_ready();
            availability.mark_mutation_ready();
        }
    }

    pub(super) fn mark_issue_tracker_unavailable(&self, reason: impl Into<String>) {
        let reason = reason.into();
        if let Some(availability) = self.issue_tracker_availability.lock().as_ref() {
            availability.mark_endpoint_unavailable(reason);
        }
    }

    pub(super) fn reset_issue_tracker_when_disabled(&self) {
        *self.issue_tracker_port.lock() = None;
        *self.issue_tracker_remote_url.lock() = None;
        *self.issue_tracker_gateway_control_url.lock() = None;
        *self.issue_tracker_gateway_worker_url.lock() = None;
        self.abort_issue_tracker_maintenance();
        self.mark_issue_tracker_unavailable("Issue Tracker is disabled");
        clear_issue_tracker_endpoint_env();
    }

    pub(super) fn warn_issue_tracker_unavailable(
        &self,
        warnings: &mut Vec<SidecarWarning>,
        message: impl Into<String>,
        detail: Option<String>,
    ) {
        let message = message.into();
        self.mark_issue_tracker_unavailable(message.clone());
        warnings.push(SidecarWarning {
            kind: SidecarWarningKind::IssueTrackerUnavailable,
            message,
            detail,
        });
    }

    pub fn issue_tracker_url(&self) -> Option<String> {
        self.issue_tracker_remote_url
            .lock()
            .clone()
            .or_else(|| self.issue_tracker_runtime_port().map(super::loopback_url))
    }

    /// Publish a remote tracker's address as jobworkerp reaches it. A missing
    /// or invalid address is a warning, like any other tracker failure.
    pub(super) fn publish_remote_issue_tracker_endpoint(
        &self,
        connection: &crate::commands::connection::ConnectionConfig,
        warnings: &mut Vec<SidecarWarning>,
    ) -> bool {
        match publish_remote_issue_tracker_endpoint_env(connection) {
            Ok(()) => true,
            Err(error) => {
                self.warn_issue_tracker_unavailable(warnings, error.to_string(), None);
                false
            }
        }
    }

    /// Connect to a remote Issue tracker instead of starting one. The
    /// remote server runs its own maintenance, so only the app's readiness
    /// checks and gateway policy are applied. Failures are warnings, as for
    /// the local tracker: the rest of the app keeps working without Issues.
    pub(super) async fn connect_remote_issue_tracker(
        &self,
        tracker: &crate::commands::connection::IssueTrackerEndpointConfig,
        warnings: &mut Vec<SidecarWarning>,
    ) {
        let (Some(url), Some(gateway_control_url)) = (
            tracker.endpoint.remote_url().map(str::to_string),
            tracker.remote_gateway_control_url.clone(),
        ) else {
            self.warn_issue_tracker_unavailable(
                warnings,
                "the remote Issue tracker URLs are not configured",
                None,
            );
            return;
        };
        self.mark_issue_tracker_unavailable("Issue Tracker startup is not yet confirmed");
        *self.issue_tracker_remote_url.lock() = Some(url.clone());
        *self.issue_tracker_gateway_control_url.lock() = Some(gateway_control_url.clone());
        *self.issue_tracker_gateway_worker_url.lock() = tracker.remote_gateway_worker_url.clone();
        let service = crate::grpc::issue_tracker::IssueTrackerGrpcService::new(url.clone());
        let result = async {
            // A remote server is already running: probe once instead of the
            // local startup wait, so a down remote does not stall startup.
            service
                .contract_probe("lookback-remote-issue-tracker-startup")
                .await?;
            if let Some(availability) = self.issue_tracker_availability.lock().as_ref() {
                availability.mark_endpoint_connected();
            }
            ensure_gateway_policy(
                &service,
                &gateway_control_url,
                &self.config.data.root,
                "lookback-remote-issue-tracker-register-gateway",
            )
            .await
        }
        .await;
        match result {
            Ok(()) => self.mark_issue_tracker_ready(),
            Err(error) => {
                self.warn_issue_tracker_unavailable(warnings, error.to_string(), Some(url))
            }
        }
    }

    pub(super) async fn start_issue_tracker_after_jobworkerp(
        &self,
        primary_port: u16,
        gateway_control_port: u16,
        gateway_worker_port: u16,
        rust_log: &str,
        issue_tracking_enabled: bool,
        warnings: &mut Vec<SidecarWarning>,
    ) -> AppResult<()> {
        if !issue_tracking_enabled {
            self.reset_issue_tracker_when_disabled();
            return Ok(());
        }
        self.mark_issue_tracker_unavailable("Issue Tracker startup is not yet confirmed");
        let Some(binary) = self.config.issue_tracker_server_bin.as_ref() else {
            self.warn_issue_tracker_unavailable(
                warnings,
                "Issue Tracker server is not configured",
                None,
            );
            return Ok(());
        };
        let binary_detail = Some(binary.display().to_string());
        let config = match crate::grpc::issue_tracker::render_server_config_with_gateway_ports(
            &self.config.data,
            primary_port,
            gateway_control_port,
            gateway_worker_port,
        ) {
            Ok(config) => config,
            Err(error) => {
                self.warn_issue_tracker_unavailable(
                    warnings,
                    error.to_string(),
                    binary_detail.clone(),
                );
                return Ok(());
            }
        };
        if let Err(error) = std::fs::write(config.config_path(), config.toml()) {
            self.warn_issue_tracker_unavailable(warnings, error.to_string(), binary_detail.clone());
            return Ok(());
        }
        *self.issue_tracker_gateway_control_url.lock() = Some(config.gateway_control_url());
        *self.issue_tracker_gateway_worker_url.lock() = Some(config.gateway_worker_url());
        let child = match self.spawn_issue_tracker_server(binary, config.config_path(), rust_log) {
            Ok(child) => child,
            Err(error) => {
                self.warn_issue_tracker_unavailable(warnings, error.to_string(), binary_detail);
                return Ok(());
            }
        };
        if let Some(processes) = self.state.lock().as_mut() {
            processes.push(Process {
                name: "issue-tracker",
                child,
                port: primary_port,
            });
        }
        let service =
            crate::grpc::issue_tracker::IssueTrackerGrpcService::new(config.primary_url());
        if let Err(error) =
            wait_for_tcp_or_failure(("127.0.0.1", primary_port), Duration::from_secs(30), None)
                .await
        {
            self.warn_issue_tracker_unavailable(
                warnings,
                error.to_string(),
                Some(binary.display().to_string()),
            );
            // Keep probing a late-starting server so the sidebar can reflect
            // recovery without changing the registered MCP tool surface.
            self.spawn_issue_tracker_maintenance(
                service,
                primary_port,
                config.gateway_control_url(),
                self.config.data.root.clone(),
            );
            return Ok(());
        }
        let startup_result = match Self::wait_for_issue_tracker_contract(
            &service,
            &config.primary_url(),
        )
        .await
        {
            Ok(_snapshot) => {
                if let Some(availability) = self.issue_tracker_availability.lock().as_ref() {
                    availability.mark_endpoint_connected();
                }
                match service
                    .run_outbox_cycle("lookback-local-issue-tracker-bootstrap")
                    .await
                {
                    Ok(_) => match service.contract_probe("lookback-local-issue-tracker-bootstrap-ready").await {
                        Ok(ready)
                            if ready.health == crate::grpc::proto::issue_tracker::manager::v1::HealthStatus::Healthy
                                && ready.activation == crate::grpc::proto::issue_tracker::manager::v1::ActivationStatus::Ready => {
                                    // Manager/outbox readiness is independent
                                    // of Gateway registration. Mark primary
                                    // mutations usable before the optional
                                    // Gateway policy handshake.
                                    mark_issue_tracker_mutation_ready(
                                        self.issue_tracker_availability.lock().as_ref(),
                                        &self.issue_tracker_port,
                                        primary_port,
                                    );
                                    ensure_gateway_policy(
                                        &service,
                                        &config.gateway_control_url(),
                                        &self.config.data.root,
                                        "lookback-local-issue-tracker-register-gateway",
                                    ).await
                                },
                        Ok(ready) => {
                            if let Some(availability) = self.issue_tracker_availability.lock().as_ref() {
                                availability.mark_mutation_unavailable(format!("manager is not ready (health={:?}, activation={:?})", ready.health, ready.activation));
                            }
                            Err(crate::error::AppError::IssueTrackerServer(format!("manager is not ready (health={:?}, activation={:?})", ready.health, ready.activation)))
                        }
                        Err(error) => Err(error),
                    },
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        match startup_result {
            Ok(_) => {
                *self.issue_tracker_port.lock() = Some(primary_port);
                self.mark_issue_tracker_ready();
                self.spawn_issue_tracker_maintenance(
                    service,
                    primary_port,
                    config.gateway_control_url(),
                    self.config.data.root.clone(),
                );
            }
            Err(error) => {
                let endpoint_connected = self
                    .issue_tracker_availability
                    .lock()
                    .as_ref()
                    .is_some_and(|availability| availability.snapshot().endpoint_connected);
                if endpoint_connected {
                    *self.issue_tracker_port.lock() = Some(primary_port);
                    warnings.push(SidecarWarning {
                        kind: SidecarWarningKind::IssueTrackerUnavailable,
                        message: error.to_string(),
                        detail: Some(binary.display().to_string()),
                    });
                } else {
                    self.warn_issue_tracker_unavailable(
                        warnings,
                        error.to_string(),
                        Some(binary.display().to_string()),
                    );
                }
                // A server can finish its own initialization just after the
                // startup budget. Keep probing it so the status can recover
                // without changing the registered MCP tool surface.
                self.spawn_issue_tracker_maintenance(
                    service,
                    primary_port,
                    config.gateway_control_url(),
                    self.config.data.root.clone(),
                );
            }
        }
        Ok(())
    }

    pub(super) fn spawn_issue_tracker_server(
        &self,
        binary: &Path,
        config_path: &Path,
        rust_log: &str,
    ) -> AppResult<Child> {
        let mut cmd = self.base_cmd(binary, &self.config.data.root, rust_log);
        cmd.arg("--config").arg(config_path);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                let scope = binary
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("issue-tracker-server");
                self.close_native_log_dirs(Some(scope));
                return Err(spawn_err("issue-tracker-server", binary, error));
            }
        };
        if let Some(pid) = child.id() {
            let scope = binary
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("issue-tracker-server");
            self.mark_native_log_dir_pid(scope, pid);
        }
        self.output_tasks.lock().extend(forward_output(
            &mut child,
            "issue-tracker",
            &self.config.data.log_dir(),
            None,
        ));
        Ok(child)
    }

    pub(super) fn spawn_issue_tracker_maintenance(
        &self,
        service: crate::grpc::issue_tracker::IssueTrackerGrpcService,
        port: u16,
        gateway_control_endpoint: String,
        data_root: PathBuf,
    ) {
        let maintenance_interval = resolve_issue_tracker_maintenance_interval(
            std::env::var(ISSUE_TRACKER_MAINTENANCE_INTERVAL_ENV_KEY)
                .ok()
                .as_deref(),
        );
        self.issue_tracker_maintenance_stop
            .store(false, Ordering::Release);
        let stop = Arc::clone(&self.issue_tracker_maintenance_stop);
        let availability = self.issue_tracker_availability.lock().clone();
        let issue_tracker_port = Arc::clone(&self.issue_tracker_port);
        let gateway_registration_lock = Arc::clone(&self.issue_tracker_gateway_registration_lock);
        let task = tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                let probe = service
                    .contract_probe("lookback-local-issue-tracker-maintenance-probe")
                    .await;
                let _snapshot = match probe {
                    Ok(snapshot) => {
                        // A successful contract probe proves that the primary
                        // endpoint can serve reads and recovery calls even if
                        // the manager/outbox remains degraded. Publish its
                        // port before those later health checks can fail.
                        *issue_tracker_port.lock() = Some(port);
                        snapshot
                    }
                    Err(error) => {
                        if let Some(availability) = availability.as_ref() {
                            availability.mark_endpoint_unavailable(error.to_string());
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                if let Some(availability) = availability.as_ref() {
                    availability.mark_endpoint_connected();
                }
                if let Err(error) = service
                    .run_outbox_cycle("lookback-local-issue-tracker-maintenance")
                    .await
                {
                    mark_issue_tracker_maintenance_unavailable(
                        availability.as_ref(),
                        &issue_tracker_port,
                        error.to_string(),
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                let ready = service
                    .contract_probe("lookback-local-issue-tracker-maintenance-ready")
                    .await;
                match ready {
                    Ok(ready)
                        if ready.health
                            == crate::grpc::proto::issue_tracker::manager::v1::HealthStatus::Healthy
                            && ready.activation
                                == crate::grpc::proto::issue_tracker::manager::v1::ActivationStatus::Ready =>
                    {
                        let _registration_guard =
                            Arc::clone(&gateway_registration_lock).lock_owned().await;
                        // Primary manager mutations only require the healthy
                        // manager and outbox. Gateway registration is a
                        // separate capability and must not mask this state.
                        mark_issue_tracker_mutation_ready(
                            availability.as_ref(),
                            &issue_tracker_port,
                            port,
                        );
                        let registration_result = ensure_gateway_policy(
                            &service,
                            &gateway_control_endpoint,
                            &data_root,
                            "lookback-local-issue-tracker-maintenance-register-gateway",
                        )
                        .await;
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        match registration_result {
                            Ok(_) => mark_issue_tracker_maintenance_ready(
                                availability.as_ref(),
                                &issue_tracker_port,
                                port,
                            ),
                            Err(error) => mark_issue_tracker_gateway_unavailable(
                                availability.as_ref(),
                                error.to_string(),
                            ),
                        }
                    }
                    Ok(ready) => mark_issue_tracker_maintenance_unavailable(
                        availability.as_ref(),
                        &issue_tracker_port,
                        format!("manager is not ready (health={:?}, activation={:?})", ready.health, ready.activation),
                    ),
                    Err(error) => {
                        if let Some(availability) = availability.as_ref() {
                            availability.mark_endpoint_unavailable(error.to_string());
                        }
                    }
                }
                tokio::time::sleep(maintenance_interval).await;
            }
        });
        *self.issue_tracker_maintenance_task.lock() = Some(task);
    }

    pub(super) async fn wait_for_issue_tracker_contract(
        service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
        endpoint: &str,
    ) -> AppResult<crate::grpc::issue_tracker::ManagerHealthSnapshot> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut attempts = 0_u32;
        loop {
            match service
                .contract_probe("lookback-local-issue-tracker-startup")
                .await
            {
                Ok(snapshot) => {
                    tracing::info!(endpoint, attempts, health = ?snapshot.health, activation = ?snapshot.activation, "Issue Tracker server manager contract is available");
                    return Ok(snapshot);
                }
                Err(error) if tokio::time::Instant::now() >= deadline => {
                    tracing::warn!(endpoint, attempts, error = %error, "Issue Tracker server did not become ready");
                    return Err(error);
                }
                Err(error) => {
                    attempts += 1;
                    if attempts == 1 {
                        tracing::info!(endpoint, error = %error, "waiting for Issue Tracker server manager readiness");
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    #[allow(dead_code)]
    pub(super) async fn wait_for_issue_tracker_health(
        service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
        endpoint: &str,
    ) -> AppResult<()> {
        let snapshot = Self::wait_for_issue_tracker_contract(service, endpoint).await?;
        if snapshot.health == crate::grpc::proto::issue_tracker::manager::v1::HealthStatus::Healthy
            && snapshot.activation
                == crate::grpc::proto::issue_tracker::manager::v1::ActivationStatus::Ready
        {
            Ok(())
        } else {
            Err(crate::error::AppError::IssueTrackerServer(format!(
                "manager is not ready (health={:?}, activation={:?})",
                snapshot.health, snapshot.activation
            )))
        }
    }

    pub(super) async fn stop_issue_tracker_maintenance(&self) {
        self.issue_tracker_maintenance_stop
            .store(true, Ordering::Release);
        let task = { self.issue_tracker_maintenance_task.lock().take() };
        if let Some(mut task) = task
            && timeout(Duration::from_secs(2), &mut task).await.is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }

    pub(super) fn abort_issue_tracker_maintenance(&self) {
        self.issue_tracker_maintenance_stop
            .store(true, Ordering::Release);
        let task = { self.issue_tracker_maintenance_task.lock().take() };
        if let Some(task) = task {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS,
        MIN_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS, issue_tracker_endpoint_env_values,
        issue_tracker_from_env, resolve_issue_tracker_maintenance_interval,
    };
    use crate::commands::connection::GrpcAddress;
    use std::time::Duration;

    #[test]
    fn the_published_tracker_reads_back_as_written() {
        let primary = GrpcAddress::loopback(39_011);
        let gateway_worker = GrpcAddress {
            host: "tracker.example".to_string(),
            port: 443,
            tls: true,
        };
        let values = issue_tracker_endpoint_env_values(&primary, &gateway_worker);
        assert_eq!(
            values.clone().map(|(_, value)| value),
            [
                "127.0.0.1",
                "39011",
                "false",
                "tracker.example",
                "443",
                "true"
            ]
            .map(str::to_string)
        );
        let env = std::collections::HashMap::from(values);
        assert_eq!(
            issue_tracker_from_env(|key| env.get(key).cloned()),
            Some(primary)
        );
        assert_eq!(issue_tracker_from_env(|_| None), None);
    }

    #[test]
    fn maintenance_interval_honors_valid_override() {
        assert_eq!(
            resolve_issue_tracker_maintenance_interval(Some("7")),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn maintenance_interval_falls_back_to_default_for_invalid_override() {
        assert_eq!(
            resolve_issue_tracker_maintenance_interval(Some("not-a-duration")),
            Duration::from_secs(DEFAULT_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS)
        );
    }

    #[test]
    fn maintenance_interval_clamps_override_below_floor() {
        assert_eq!(
            resolve_issue_tracker_maintenance_interval(Some("0")),
            Duration::from_secs(MIN_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS)
        );
    }

    #[test]
    fn maintenance_interval_trims_valid_override() {
        assert_eq!(
            resolve_issue_tracker_maintenance_interval(Some(" 3 ")),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn maintenance_interval_falls_back_to_default_for_empty_override() {
        assert_eq!(
            resolve_issue_tracker_maintenance_interval(Some("")),
            Duration::from_secs(DEFAULT_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS)
        );
    }

    #[test]
    fn maintenance_interval_accepts_maximum_u64_override() {
        assert_eq!(
            resolve_issue_tracker_maintenance_interval(Some("18446744073709551615")),
            Duration::from_secs(u64::MAX)
        );
    }
}
