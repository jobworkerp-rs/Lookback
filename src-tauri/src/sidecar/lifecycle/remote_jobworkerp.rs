//! Lookback's workers on a remote jobworkerp.
//!
//! The app registers its workers only on the jobworkerp it runs. A remote
//! jobworkerp gets them from the connection settings, through the same
//! registration steps with the addresses the remote side uses. Its channels,
//! plugins and environment are the remote operator's, so they are checked
//! and explained rather than changed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::future::join_all;
use serde::Serialize;

use super::{
    LOCAL_ONLY_CHANNELS, SidecarWarning, SidecarWarningKind, Sidecars, channel_env_vars,
    current_embedding_space, experimental_issues_env_vars, issue_tool_registration,
    issue_worker_yamls, jobworkerp_channels, jobworkerp_workflow_env_vars,
    parse_expanded_worker_yaml, register_runtime_memories_dependent_workers,
    register_runtime_workers_before_memories, register_staged_function_sets, resolve_timezone,
};
use crate::commands::connection::{self, ConnectionConfig};
use crate::error::{AppError, AppResult};
use crate::jobworkerp::JobworkerpHandle;

/// What a remote jobworkerp still lacks for Lookback.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RemoteJobworkerpReadiness {
    pub jobworkerp_url: String,
    pub missing_workers: Vec<String>,
    pub missing_function_sets: Vec<String>,
    /// The workers were registered by another version of the app, or never
    /// from this app.
    pub registration_outdated: bool,
    pub missing_channels: Vec<String>,
    pub missing_runners: Vec<String>,
    /// Environment the remote jobworkerp process needs, as `KEY=value`.
    pub environment: Vec<String>,
}

impl RemoteJobworkerpReadiness {
    pub fn is_ready(&self) -> bool {
        self.missing_workers.is_empty()
            && self.missing_function_sets.is_empty()
            && !self.registration_outdated
            && self.missing_channels.is_empty()
            && self.missing_runners.is_empty()
    }

    pub(super) fn warning(&self) -> SidecarWarning {
        let mut gaps = Vec::new();
        if !self.missing_workers.is_empty()
            || !self.missing_function_sets.is_empty()
            || self.registration_outdated
        {
            gaps.push("Lookback の worker の登録・更新");
        }
        if !self.missing_channels.is_empty() {
            gaps.push("channel の設定");
        }
        if !self.missing_runners.is_empty() {
            gaps.push("plugin の配置");
        }
        remote_warning(
            SidecarWarningKind::RemoteJobworkerpUnprepared,
            &self.jobworkerp_url,
            &format!("に準備が必要です: {}", gaps.join("、")),
            None,
        )
    }
}

/// How long the startup check of a remote jobworkerp may take. Its result is
/// only a warning, so an unresponsive server must not hold up the start.
const STARTUP_CHECK_TIMEOUT: Duration = Duration::from_secs(15);

fn remote_warning(
    kind: SidecarWarningKind,
    url: &str,
    problem: &str,
    detail: Option<String>,
) -> SidecarWarning {
    SidecarWarning {
        kind,
        message: format!("リモートの jobworkerp（{url}）{problem}。接続設定から確認してください"),
        detail,
    }
}

fn unavailable_warning(url: &str, error: &AppError) -> SidecarWarning {
    remote_warning(
        SidecarWarningKind::RemoteJobworkerpUnavailable,
        url,
        "に接続できません",
        Some(error.to_string()),
    )
}

fn inspection_failed_warning(url: &str, error: &AppError) -> SidecarWarning {
    remote_warning(
        SidecarWarningKind::RemoteJobworkerpUnprepared,
        url,
        "の準備状況を確認できません",
        Some(error.to_string()),
    )
}

/// The worker manifests as registered on a remote jobworkerp, expanded once
/// for both the worker names and the fingerprint.
struct RemoteManifests {
    paths: Vec<PathBuf>,
    worker_names: Vec<String>,
    fingerprint: String,
}

impl RemoteManifests {
    /// The chat Issue tools call a facade inside the app on loopback, which a
    /// remote host cannot reach, so their manifest is left out.
    fn load(base: &[PathBuf], issue_tracking_enabled: bool) -> AppResult<Self> {
        use sha2::{Digest, Sha256};
        let paths = issue_worker_yamls(base, issue_tracking_enabled);
        let mut worker_names = crate::sidecar::generation_workers::generation_worker_names();
        let mut hasher = Sha256::new();
        hasher.update(env!("CARGO_PKG_VERSION"));
        for yaml in &paths {
            let document = parse_expanded_worker_yaml(yaml)?;
            worker_names.extend(
                document["workers"]
                    .as_sequence()
                    .into_iter()
                    .flatten()
                    .filter_map(|worker| worker["name"].as_str().map(str::to_string)),
            );
            hasher.update(serde_yaml::to_string(&document).unwrap_or_default());
        }
        Ok(Self {
            paths,
            worker_names,
            fingerprint: format!("{:x}", hasher.finalize()),
        })
    }
}

fn read_registration_record(path: &Path) -> BTreeMap<String, String> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Remember what was registered on `url`, keeping the other jobworkerps.
fn record_registration(path: &Path, url: &str, fingerprint: &str) -> AppResult<()> {
    let mut record = read_registration_record(path);
    record.insert(url.to_string(), fingerprint.to_string());
    let json = serde_json::to_vec_pretty(&record)
        .map_err(|e| AppError::Config(format!("serialize registration record: {e}")))?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Every channel Lookback configures on its own jobworkerp except the coding
/// agent's, which stays local.
fn remote_channels() -> Vec<(&'static str, u32)> {
    jobworkerp_channels()
        .into_iter()
        .filter(|(name, _)| !LOCAL_ONLY_CHANNELS.contains(name))
        .collect()
}

/// The environment a remote jobworkerp process needs for Lookback's workers,
/// as `KEY=value` lines for its operator: the channels, the task timeout, the
/// time zone the date logic uses and the Issue feature switch.
fn remote_environment(data: &crate::data::DataPaths) -> Vec<String> {
    let app_settings = crate::data::paths::load_app_settings(&data.app_settings_path());
    let issues_enabled =
        crate::commands::issue_tracker_settings::effective_issue_tracking_enabled(data);
    channel_env_vars(&remote_channels())
        .into_iter()
        .chain(
            jobworkerp_workflow_env_vars()
                .into_iter()
                .chain(experimental_issues_env_vars(issues_enabled))
                .map(|(key, value)| (key, value.to_string())),
        )
        .chain([("TZ", resolve_timezone(Some(&app_settings)))])
        .map(|(key, value)| format!("{key}={value}"))
        .collect()
}

/// Plugin runners Lookback's workers run on, beside jobworkerp's built-ins.
fn plugin_runners() -> [&'static str; 2] {
    [
        crate::jobworkerp::LLM_PROMPT_RUNNER_NAME,
        crate::commands::embedding_workers_yaml::EMBEDDING_RUNNER_NAME,
    ]
}

/// The names in `names` that `exists` reports absent, asked concurrently.
async fn absent<F, Fut>(
    names: impl IntoIterator<Item = String>,
    exists: F,
) -> AppResult<Vec<String>>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = AppResult<bool>>,
{
    let names = names.into_iter().collect::<Vec<_>>();
    let found = join_all(names.iter().cloned().map(exists)).await;
    names
        .into_iter()
        .zip(found)
        .filter_map(|(name, found)| match found {
            Ok(true) => None,
            Ok(false) => Some(Ok(name)),
            Err(error) => Some(Err(error)),
        })
        .collect()
}

impl Sidecars {
    /// Prepare this process's environment for expanding the manifests with
    /// the addresses the remote side uses, and load those manifests. Writing
    /// the environment is a side effect even of a read-only inspection:
    /// expansion reads it, and the next registration needs the same values.
    fn remote_manifests(&self, connection: &ConnectionConfig) -> AppResult<RemoteManifests> {
        let issue_tracking_enabled =
            crate::commands::issue_tracker_settings::effective_issue_tracking_enabled(
                &self.config.data,
            );
        connection::validate_connection_config(connection, issue_tracking_enabled)?;
        // From the connection, not from what startup published: an earlier
        // single-mode config starts with a local tracker, which publishes
        // nothing a remote jobworkerp could use.
        if issue_tracking_enabled {
            super::issue_tracker::publish_remote_issue_tracker_endpoint_env(connection)?;
        }
        self.apply_worker_registration_env(&connection::remote_memories_for_jobworkerp(
            connection,
        )?)?;
        RemoteManifests::load(&self.config.worker_yaml_paths, issue_tracking_enabled)
    }

    fn remote_jobworkerp_target(&self) -> AppResult<(ConnectionConfig, String)> {
        let connection =
            connection::load_connection_config(&self.config.data.connection_config_path());
        let url = connection
            .jobworkerp
            .active_remote_url()
            .ok_or_else(|| AppError::Config("jobworkerp is not set to remote".into()))?
            .to_string();
        Ok((connection, url))
    }

    /// What to warn about the remote jobworkerp at startup: unreachable,
    /// unprepared or not inspectable. Never fails the start: the app has to
    /// open so the connection can be changed in Settings.
    pub(super) async fn remote_jobworkerp_startup_warning(
        &self,
        connection: &ConnectionConfig,
        url: &str,
    ) -> Option<SidecarWarning> {
        let check = async {
            let handle = match JobworkerpHandle::connect(url).await {
                Ok(handle) => handle,
                Err(error) => {
                    let error = crate::commands::jobworkerp_connect_error(url, error);
                    return Some(unavailable_warning(url, &error));
                }
            };
            match self
                .inspect_remote_jobworkerp_with(&handle, connection, url)
                .await
            {
                Ok(readiness) => (!readiness.is_ready()).then(|| readiness.warning()),
                Err(error) => Some(inspection_failed_warning(url, &error)),
            }
        };
        tokio::time::timeout(STARTUP_CHECK_TIMEOUT, check)
            .await
            .unwrap_or_else(|_| {
                Some(unavailable_warning(
                    url,
                    &AppError::Config(format!(
                        "no response within {}s",
                        STARTUP_CHECK_TIMEOUT.as_secs()
                    )),
                ))
            })
    }

    /// Check the configured remote jobworkerp for what Lookback needs.
    pub async fn inspect_remote_jobworkerp(&self) -> AppResult<RemoteJobworkerpReadiness> {
        let (connection, url) = self.remote_jobworkerp_target()?;
        let handle = JobworkerpHandle::connect(&url)
            .await
            .map_err(|error| crate::commands::jobworkerp_connect_error(&url, error))?;
        self.inspect_remote_jobworkerp_with(&handle, &connection, &url)
            .await
    }

    pub(super) async fn inspect_remote_jobworkerp_with(
        &self,
        handle: &JobworkerpHandle,
        connection: &ConnectionConfig,
        url: &str,
    ) -> AppResult<RemoteJobworkerpReadiness> {
        let manifests = self.remote_manifests(connection)?;
        self.readiness(handle, &manifests, url).await
    }

    async fn readiness(
        &self,
        handle: &JobworkerpHandle,
        manifests: &RemoteManifests,
        url: &str,
    ) -> AppResult<RemoteJobworkerpReadiness> {
        let workers = absent(manifests.worker_names.iter().cloned(), |name| async move {
            Ok(handle.worker_id_by_name(&name).await?.is_some())
        });
        let function_sets = absent(
            issue_tool_registration::REQUIRED_FUNCTION_SET_NAMES.map(str::to_string),
            |name| async move { handle.function_set_exists(&name).await },
        );
        let runners = absent(plugin_runners().map(str::to_string), |name| async move {
            handle.runner_exists(&name).await
        });
        let (missing_workers, missing_function_sets, missing_runners, channels) =
            tokio::join!(workers, function_sets, runners, handle.processed_channels());
        let channels = channels?.into_iter().collect::<BTreeSet<_>>();
        let recorded =
            read_registration_record(&self.config.data.remote_jobworkerp_registrations_path());
        Ok(RemoteJobworkerpReadiness {
            jobworkerp_url: url.to_string(),
            missing_workers: missing_workers?,
            missing_function_sets: missing_function_sets?,
            registration_outdated: recorded.get(url) != Some(&manifests.fingerprint),
            missing_channels: remote_channels()
                .into_iter()
                .map(|(name, _)| name.to_string())
                .filter(|name| !channels.contains(name))
                .collect(),
            missing_runners: missing_runners?,
            environment: remote_environment(&self.config.data),
        })
    }

    /// Register Lookback's workers and function sets on the configured
    /// remote jobworkerp, then check it again. Steps that failed without
    /// stopping the rest come back as warnings.
    pub async fn register_remote_jobworkerp_workers(
        &self,
    ) -> AppResult<(Vec<SidecarWarning>, RemoteJobworkerpReadiness)> {
        let (connection, url) = self.remote_jobworkerp_target()?;
        let manifests = self.remote_manifests(&connection)?;
        let handle = JobworkerpHandle::connect(&url)
            .await
            .map_err(|error| crate::commands::jobworkerp_connect_error(&url, error))?;
        let mut warnings = Vec::new();
        // Failures stay warnings so one broken manifest leaves the rest.
        register_runtime_workers_before_memories(&handle, &manifests.paths, true, &mut warnings)
            .await?;
        crate::sidecar::generation_workers::register_generation_workers(
            &url,
            &self.config.data.log_dir(),
            &mut warnings,
        )
        .await;
        let embedding_space =
            current_embedding_space(&connection::memories_url(&connection, None)?).await?;
        let query_embedding = crate::sidecar::query_embedding_worker::prepare_query_embedding(
            &handle,
            &embedding_space,
            &self.current_embedding_runtime(),
            self.config.workflows_dir.as_deref(),
            &mut warnings,
        )
        .await;
        register_runtime_memories_dependent_workers(
            &handle,
            &manifests.paths,
            &query_embedding,
            &connection::jobworkerp_for_dependents(&connection, None)?,
            true,
            &mut warnings,
        )
        .await?;
        // The function sets go without Issue tools: their chat side is the
        // loopback facade. Issue extraction and resolution workers stay.
        let staged = self
            .config
            .function_set_yaml_paths
            .iter()
            .map(|source| {
                issue_tool_registration::stage_function_set_manifest(
                    source,
                    &self.config.data.root,
                    false,
                )
            })
            .collect::<AppResult<Vec<_>>>()?;
        register_staged_function_sets(&handle, &staged, true, &mut warnings).await?;
        if warnings.is_empty() {
            record_registration(
                &self.config.data.remote_jobworkerp_registrations_path(),
                &url,
                &manifests.fingerprint,
            )?;
        }
        let readiness = self.readiness(&handle, &manifests, &url).await?;
        Ok((warnings, readiness))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreachable_remote_jobworkerp_is_its_own_warning() {
        let warning = unavailable_warning(
            "http://j:9000",
            &AppError::Config("jobworkerp connection failed (http://j:9000): refused".into()),
        );
        assert_eq!(
            warning.kind,
            SidecarWarningKind::RemoteJobworkerpUnavailable
        );
        assert!(warning.message.contains("http://j:9000"));
        assert!(warning.detail.unwrap().contains("refused"));
    }

    #[test]
    fn a_failed_inspection_names_the_reason() {
        let warning = inspection_failed_warning(
            "http://j:9000",
            &AppError::Config("a remote jobworkerp needs a remote Issue tracker".into()),
        );
        assert_eq!(warning.kind, SidecarWarningKind::RemoteJobworkerpUnprepared);
        assert!(warning.message.contains("http://j:9000"));
        assert!(warning.detail.unwrap().contains("remote Issue tracker"));
    }

    #[test]
    fn a_remote_needs_every_channel_but_the_coding_agents() {
        let channels = remote_channels()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        for channel in ["llm", "llm_batch", "llm_workflow", "rag", "embedding"] {
            assert!(channels.contains(&channel), "{channel}");
        }
        for channel in LOCAL_ONLY_CHANNELS {
            assert!(!channels.contains(&channel), "{channel}");
        }
    }

    #[test]
    fn readiness_counts_every_gap() {
        assert!(RemoteJobworkerpReadiness::default().is_ready());
        for gap in [
            RemoteJobworkerpReadiness {
                registration_outdated: true,
                ..Default::default()
            },
            RemoteJobworkerpReadiness {
                missing_function_sets: vec!["lookback-chat".into()],
                ..Default::default()
            },
            RemoteJobworkerpReadiness {
                missing_channels: vec!["rag".into()],
                ..Default::default()
            },
        ] {
            assert!(!gap.is_ready(), "{gap:?}");
        }
    }

    #[test]
    fn a_registration_keeps_the_other_jobworkerps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote-jobworkerp-registrations.json");
        assert!(read_registration_record(&path).is_empty());
        record_registration(&path, "https://a.example", "one").unwrap();
        record_registration(&path, "https://b.example", "two").unwrap();
        record_registration(&path, "https://a.example", "three").unwrap();
        assert_eq!(
            read_registration_record(&path),
            BTreeMap::from([
                ("https://a.example".to_string(), "three".to_string()),
                ("https://b.example".to_string(), "two".to_string()),
            ])
        );
    }

    #[tokio::test]
    async fn absent_lists_the_names_reported_missing() {
        let missing = absent(["a".to_string(), "b".to_string()], |name| async move {
            Ok(name == "a")
        })
        .await
        .unwrap();
        assert_eq!(missing, ["b"]);
    }
}
