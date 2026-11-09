//! Read-only repository host verification at the Tauri/native boundary.

use std::{future::Future, pin::Pin};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    RepositoryHostConfig, RepositoryIdentity, RepositoryResolution,
    settings::{CodingAgentSettingsStore, RepositoryHostSettings, RepositoryIdentitySettings},
};
use crate::{
    coding_agent::executor_client::{NativeDiagnosticTransportReason, NativeExecutorError},
    data::DataPaths,
    native_terminal::bootstrap::{
        NativeBootstrapUnavailable, NativePackageSelection, NativeRepositoryCheckError,
        execute_repository_check, read_repository_policy_for_diagnostic,
    },
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStageName {
    Configuration,
    TrustedPolicy,
    GitRead,
    ForgeApi,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStageStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckCode {
    SettingsValid,
    InvalidSettings,
    RevisionConflict,
    PolicyMatched,
    RepositoryRequired,
    PolicyMissing,
    PolicyInvalid,
    RepositoryNotApproved,
    PolicyMismatch,
    GitReadable,
    GitFailed,
    ForgeReadable,
    ForgeNotConfigured,
    CredentialNotConfigured,
    NativeBackendNotApproved,
    AuthenticationFailed,
    AccessDenied,
    RepositoryNotFound,
    RedirectRejected,
    InvalidResponse,
    CredentialUnavailable,
    Timeout,
    ResponseTooLarge,
    Unavailable,
    UnsupportedCapability,
    PrerequisiteFailed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryHostCheckStage {
    pub stage: CheckStageName,
    pub status: CheckStageStatus,
    pub code: CheckCode,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryHostCheckRequest {
    pub expected_revision: u64,
    pub host: RepositoryHostSettings,
    pub repository: Option<RepositoryIdentitySettings>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryHostCheckResponse {
    pub settings_revision: u64,
    pub repository: Option<RepositoryIdentitySettings>,
    pub stages: Vec<RepositoryHostCheckStage>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StructuralCheckError {
    InvalidSettings,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedRepositoryHost {
    pub(crate) host: RepositoryHostConfig,
    pub(crate) repository: Option<RepositoryIdentity>,
    pub(crate) clone_url: Option<String>,
}

#[derive(Debug, Error)]
pub(crate) enum RepositoryHostCheckError {
    #[error("coding-agent settings are unavailable")]
    SettingsUnavailable,
}

type RepositoryCheckFuture<'a> = Pin<
    Box<
        dyn Future<
                Output = Result<
                    coding_agent_executor::RepositoryCheckResult,
                    NativeRepositoryCheckError,
                >,
            > + Send
            + 'a,
    >,
>;

trait RepositoryCheckRunner {
    fn run<'a>(
        &'a self,
        data_paths: &'a DataPaths,
        package: &'a NativePackageSelection,
        expected_policy_sha256: &'a str,
        repository: &'a coding_agent_executor::RepositoryId,
    ) -> RepositoryCheckFuture<'a>;
}

struct ProductionRepositoryCheckRunner;

impl RepositoryCheckRunner for ProductionRepositoryCheckRunner {
    fn run<'a>(
        &'a self,
        data_paths: &'a DataPaths,
        package: &'a NativePackageSelection,
        expected_policy_sha256: &'a str,
        repository: &'a coding_agent_executor::RepositoryId,
    ) -> RepositoryCheckFuture<'a> {
        Box::pin(execute_repository_check(
            data_paths,
            package,
            expected_policy_sha256,
            repository,
        ))
    }
}

pub(crate) async fn check_repository_host(
    data_paths: &DataPaths,
    settings: &CodingAgentSettingsStore,
    package: Option<&NativePackageSelection>,
    package_error: Option<NativeBootstrapUnavailable>,
    request: RepositoryHostCheckRequest,
) -> Result<RepositoryHostCheckResponse, RepositoryHostCheckError> {
    let runner = ProductionRepositoryCheckRunner;
    check_repository_host_with_runner(
        data_paths,
        settings,
        package,
        package_error,
        request,
        &runner,
    )
    .await
}

async fn check_repository_host_with_runner<R: RepositoryCheckRunner + ?Sized>(
    data_paths: &DataPaths,
    settings: &CodingAgentSettingsStore,
    package: Option<&NativePackageSelection>,
    package_error: Option<NativeBootstrapUnavailable>,
    request: RepositoryHostCheckRequest,
    runner: &R,
) -> Result<RepositoryHostCheckResponse, RepositoryHostCheckError> {
    let current = settings
        .load()
        .map_err(|_| RepositoryHostCheckError::SettingsUnavailable)?;
    let repository = request.repository.clone();
    if current.revision != request.expected_revision {
        return Ok(response(
            current.revision,
            repository,
            revision_conflict_stages(),
        ));
    }
    let validated = match validate_request_shape(&request.host, request.repository.as_ref()) {
        Ok(validated) => validated,
        Err(StructuralCheckError::InvalidSettings) => {
            return Ok(response(
                current.revision,
                repository,
                invalid_settings_stages(),
            ));
        }
    };
    let Some(repository_identity) = validated.repository.as_ref() else {
        return Ok(response(
            current.revision,
            repository,
            stages_for_missing_repository(current.revision),
        ));
    };
    // A renderer draft is not an authority source. Only the exact host
    // snapshot persisted at this revision may reach the native checker.
    if !current
        .repository_hosts
        .iter()
        .any(|saved| saved == &request.host)
    {
        return Ok(response(
            current.revision,
            repository,
            policy_failed_stages(CheckCode::PolicyMismatch),
        ));
    }
    let Some(package) = package else {
        let code = package_error_code(package_error);
        return Ok(response(
            current.revision,
            repository,
            policy_failed_stages(code),
        ));
    };
    if current.native_backend_package_fingerprint.as_deref() != Some(package.package_fingerprint())
    {
        return Ok(response(
            current.revision,
            repository,
            policy_failed_stages(CheckCode::NativeBackendNotApproved),
        ));
    }
    // Display-only precheck: the runner re-hashes artifacts before executing.
    let policy = match read_repository_policy_for_diagnostic(data_paths, package) {
        Ok(policy) => policy,
        Err(error) => {
            return Ok(response(
                current.revision,
                repository,
                policy_failed_stages(policy_error_code(error)),
            ));
        }
    };
    let executor_repository = repository_identity_to_executor(repository_identity);
    let Some(approved) = policy
        .repositories
        .iter()
        .find(|entry| entry.repository == executor_repository)
    else {
        return Ok(response(
            current.revision,
            repository,
            policy_failed_stages(CheckCode::RepositoryNotApproved),
        ));
    };
    if !approved.matches_host(
        &validated.host,
        &executor_repository,
        validated.clone_url.as_deref().unwrap_or_default(),
    ) {
        return Ok(response(
            current.revision,
            repository,
            policy_failed_stages(CheckCode::PolicyMismatch),
        ));
    }
    let mut stages = vec![
        stage(
            CheckStageName::Configuration,
            CheckStageStatus::Passed,
            CheckCode::SettingsValid,
        ),
        stage(
            CheckStageName::TrustedPolicy,
            CheckStageStatus::Passed,
            CheckCode::PolicyMatched,
        ),
    ];
    match runner
        .run(
            data_paths,
            package,
            &policy.policy_sha256,
            &executor_repository,
        )
        .await
    {
        Ok(result) => {
            stages.extend(result_stages(result));
        }
        Err(error) => {
            stages.extend(failed_executor_stages(error));
        }
    }
    let latest = settings
        .load()
        .map_err(|_| RepositoryHostCheckError::SettingsUnavailable)?;
    if latest.revision != request.expected_revision {
        return Ok(response(
            latest.revision,
            repository,
            revision_conflict_stages(),
        ));
    }
    Ok(response(current.revision, repository, stages))
}

fn response(
    settings_revision: u64,
    repository: Option<RepositoryIdentitySettings>,
    stages: Vec<RepositoryHostCheckStage>,
) -> RepositoryHostCheckResponse {
    RepositoryHostCheckResponse {
        settings_revision,
        repository,
        stages,
    }
}

fn repository_identity_to_executor(
    repository: &RepositoryIdentity,
) -> coding_agent_executor::RepositoryId {
    coding_agent_executor::RepositoryId {
        host: repository.host.clone(),
        owner: repository.organization.clone(),
        name: repository.repository.clone(),
    }
}

fn invalid_settings_stages() -> Vec<RepositoryHostCheckStage> {
    vec![
        stage(
            CheckStageName::Configuration,
            CheckStageStatus::Failed,
            CheckCode::InvalidSettings,
        ),
        prerequisite_stage(CheckStageName::TrustedPolicy),
        prerequisite_stage(CheckStageName::GitRead),
        prerequisite_stage(CheckStageName::ForgeApi),
    ]
}

fn revision_conflict_stages() -> Vec<RepositoryHostCheckStage> {
    vec![
        stage(
            CheckStageName::Configuration,
            CheckStageStatus::Failed,
            CheckCode::RevisionConflict,
        ),
        prerequisite_stage(CheckStageName::TrustedPolicy),
        prerequisite_stage(CheckStageName::GitRead),
        prerequisite_stage(CheckStageName::ForgeApi),
    ]
}

fn policy_failed_stages(code: CheckCode) -> Vec<RepositoryHostCheckStage> {
    vec![
        stage(
            CheckStageName::Configuration,
            CheckStageStatus::Passed,
            CheckCode::SettingsValid,
        ),
        stage(
            CheckStageName::TrustedPolicy,
            CheckStageStatus::Failed,
            code,
        ),
        prerequisite_stage(CheckStageName::GitRead),
        prerequisite_stage(CheckStageName::ForgeApi),
    ]
}

fn prerequisite_stage(stage_name: CheckStageName) -> RepositoryHostCheckStage {
    stage(
        stage_name,
        CheckStageStatus::Skipped,
        CheckCode::PrerequisiteFailed,
    )
}

fn result_stages(
    result: coding_agent_executor::RepositoryCheckResult,
) -> Vec<RepositoryHostCheckStage> {
    result
        .stages
        .into_iter()
        .map(|stage_result| {
            let fallback_stage = match stage_result.stage.as_str() {
                "forge_api" => CheckStageName::ForgeApi,
                _ => CheckStageName::GitRead,
            };
            let (stage_name, status, code) = match (
                stage_result.stage.as_str(),
                stage_result.status.as_str(),
                stage_result.code.as_str(),
            ) {
                ("git_read", "passed", "git_readable") => (
                    CheckStageName::GitRead,
                    CheckStageStatus::Passed,
                    CheckCode::GitReadable,
                ),
                ("git_read", "failed", "git_failed") => (
                    CheckStageName::GitRead,
                    CheckStageStatus::Failed,
                    CheckCode::GitFailed,
                ),
                ("git_read", "failed", "timeout") => (
                    CheckStageName::GitRead,
                    CheckStageStatus::Failed,
                    CheckCode::Timeout,
                ),
                ("git_read", "failed", "response_too_large") => (
                    CheckStageName::GitRead,
                    CheckStageStatus::Failed,
                    CheckCode::ResponseTooLarge,
                ),
                ("git_read", "failed", "credential_unavailable") => (
                    CheckStageName::GitRead,
                    CheckStageStatus::Failed,
                    CheckCode::CredentialUnavailable,
                ),
                ("git_read", "failed", "unavailable") => (
                    CheckStageName::GitRead,
                    CheckStageStatus::Failed,
                    CheckCode::Unavailable,
                ),
                ("git_read", "skipped", "prerequisite_failed") => (
                    CheckStageName::GitRead,
                    CheckStageStatus::Skipped,
                    CheckCode::PrerequisiteFailed,
                ),
                ("forge_api", "passed", "forge_readable") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Passed,
                    CheckCode::ForgeReadable,
                ),
                ("forge_api", "failed", "forge_not_configured") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Failed,
                    CheckCode::ForgeNotConfigured,
                ),
                ("forge_api", "skipped", "forge_not_configured") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Skipped,
                    CheckCode::ForgeNotConfigured,
                ),
                ("forge_api", "failed", "credential_not_configured") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Failed,
                    CheckCode::CredentialNotConfigured,
                ),
                ("forge_api", "skipped", "credential_not_configured") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Skipped,
                    CheckCode::CredentialNotConfigured,
                ),
                ("forge_api", "failed", "credential_unavailable") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Failed,
                    CheckCode::CredentialUnavailable,
                ),
                ("forge_api", "failed", "authentication_failed") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Failed,
                    CheckCode::AuthenticationFailed,
                ),
                ("forge_api", "failed", "access_denied") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Failed,
                    CheckCode::AccessDenied,
                ),
                ("forge_api", "failed", "repository_not_found") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Failed,
                    CheckCode::RepositoryNotFound,
                ),
                ("forge_api", "failed", "redirect_rejected") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Failed,
                    CheckCode::RedirectRejected,
                ),
                ("forge_api", "failed", "invalid_response") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Failed,
                    CheckCode::InvalidResponse,
                ),
                ("forge_api", "failed", "timeout") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Failed,
                    CheckCode::Timeout,
                ),
                ("forge_api", "failed", "response_too_large") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Failed,
                    CheckCode::ResponseTooLarge,
                ),
                ("forge_api", "failed", "unavailable") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Failed,
                    CheckCode::Unavailable,
                ),
                ("forge_api", "skipped", "prerequisite_failed") => (
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Skipped,
                    CheckCode::PrerequisiteFailed,
                ),
                _ => (
                    fallback_stage,
                    CheckStageStatus::Failed,
                    CheckCode::Unavailable,
                ),
            };
            stage(stage_name, status, code)
        })
        .collect()
}

fn failed_executor_stages(error: NativeRepositoryCheckError) -> Vec<RepositoryHostCheckStage> {
    let code = match error {
        NativeRepositoryCheckError::PolicyChanged => CheckCode::PolicyMismatch,
        NativeRepositoryCheckError::UnsupportedCapability => CheckCode::UnsupportedCapability,
        NativeRepositoryCheckError::Bootstrap(error) => policy_error_code(error),
        NativeRepositoryCheckError::Executor(NativeExecutorError::UnsupportedCapability {
            ..
        }) => CheckCode::UnsupportedCapability,
        NativeRepositoryCheckError::Executor(NativeExecutorError::DiagnosticTransport {
            reason,
        }) => match reason {
            NativeDiagnosticTransportReason::Timeout => CheckCode::Timeout,
            NativeDiagnosticTransportReason::ResponseTooLarge => CheckCode::ResponseTooLarge,
            NativeDiagnosticTransportReason::Unavailable => CheckCode::Unavailable,
        },
        NativeRepositoryCheckError::Executor(NativeExecutorError::ExecutorRejected(code)) => {
            match code {
                coding_agent_executor::ExecutorErrorCode::ResponseTooLarge => {
                    CheckCode::ResponseTooLarge
                }
                coding_agent_executor::ExecutorErrorCode::GitOperationFailed => {
                    CheckCode::GitFailed
                }
                _ => CheckCode::Unavailable,
            }
        }
        NativeRepositoryCheckError::Executor(_) => CheckCode::Unavailable,
    };
    vec![
        stage(CheckStageName::GitRead, CheckStageStatus::Failed, code),
        prerequisite_stage(CheckStageName::ForgeApi),
    ]
}

fn package_error_code(error: Option<NativeBootstrapUnavailable>) -> CheckCode {
    error.map_or(CheckCode::Unavailable, policy_error_code)
}

fn policy_error_code(error: NativeBootstrapUnavailable) -> CheckCode {
    match error {
        NativeBootstrapUnavailable::TrustedPolicyMissing => CheckCode::PolicyMissing,
        NativeBootstrapUnavailable::InvalidTrustedPolicy => CheckCode::PolicyInvalid,
        NativeBootstrapUnavailable::UnsupportedPlatform
        | NativeBootstrapUnavailable::PackageUnavailable
        | NativeBootstrapUnavailable::PackageIdentityMismatch
        | NativeBootstrapUnavailable::StorageUnavailable
        | NativeBootstrapUnavailable::ExecutorUnavailable => CheckCode::Unavailable,
        NativeBootstrapUnavailable::RepositoryNotApproved => CheckCode::RepositoryNotApproved,
        NativeBootstrapUnavailable::CredentialUnavailable => CheckCode::CredentialUnavailable,
        NativeBootstrapUnavailable::UnsafeTrustedPolicy => CheckCode::PolicyInvalid,
    }
}

pub(crate) fn validate_request_shape(
    host: &RepositoryHostSettings,
    repository: Option<&RepositoryIdentitySettings>,
) -> Result<ValidatedRepositoryHost, StructuralCheckError> {
    let host_config = host
        .to_repository_host_config()
        .map_err(|_| StructuralCheckError::InvalidSettings)?;
    host_config
        .validate()
        .map_err(|_| StructuralCheckError::InvalidSettings)?;
    let Some(repository) = repository else {
        return Ok(ValidatedRepositoryHost {
            host: host_config,
            repository: None,
            clone_url: None,
        });
    };
    let identity = repository
        .to_repository_identity()
        .map_err(|_| StructuralCheckError::InvalidSettings)?;
    if identity.host != host_config.label_host {
        return Err(StructuralCheckError::InvalidSettings);
    }
    let source_label = format!(
        "repo:{}/{}/{}",
        identity.host, identity.organization, identity.repository
    );
    let resolved = match super::resolve_repository(
        &[source_label],
        std::slice::from_ref(&host_config),
        None,
    ) {
        RepositoryResolution::Ready(repository) => repository,
        RepositoryResolution::RequiresSelection { .. } => {
            return Err(StructuralCheckError::InvalidSettings);
        }
    };
    Ok(ValidatedRepositoryHost {
        host: host_config,
        repository: Some(identity),
        clone_url: Some(resolved.clone_url),
    })
}

pub(crate) fn stages_for_missing_repository(
    _settings_revision: u64,
) -> Vec<RepositoryHostCheckStage> {
    vec![
        stage(
            CheckStageName::Configuration,
            CheckStageStatus::Passed,
            CheckCode::SettingsValid,
        ),
        stage(
            CheckStageName::TrustedPolicy,
            CheckStageStatus::Skipped,
            CheckCode::RepositoryRequired,
        ),
        stage(
            CheckStageName::GitRead,
            CheckStageStatus::Skipped,
            CheckCode::PrerequisiteFailed,
        ),
        stage(
            CheckStageName::ForgeApi,
            CheckStageStatus::Skipped,
            CheckCode::PrerequisiteFailed,
        ),
    ]
}

fn stage(
    stage: CheckStageName,
    status: CheckStageStatus,
    code: CheckCode,
) -> RepositoryHostCheckStage {
    RepositoryHostCheckStage {
        stage,
        status,
        code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding_agent::settings::{
        CloneTransportSettings, CodingAgentSettings, CodingAgentSettingsStore,
        RepositoryHostSettings,
    };
    use coding_agent_executor::{EXECUTOR_VERSION, PROTOCOL_VERSION};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::oneshot;

    fn host() -> RepositoryHostSettings {
        RepositoryHostSettings {
            label_host: "github.com".into(),
            forge: crate::coding_agent::ForgeKind::GitHub,
            api_base_url: "https://api.github.com".into(),
            clone_transport: CloneTransportSettings::Https {
                base_url: "https://github.com".into(),
            },
            read_credential_ref: Some("read".into()),
            publication_credential_ref: Some("publish".into()),
        }
    }

    fn repository() -> RepositoryIdentitySettings {
        RepositoryIdentitySettings {
            host: "github.com".into(),
            organization: "acme".into(),
            repository: "widget".into(),
        }
    }

    fn request(repository: Option<RepositoryIdentitySettings>) -> RepositoryHostCheckRequest {
        RepositoryHostCheckRequest {
            expected_revision: 2,
            host: host(),
            repository,
        }
    }

    #[derive(Debug)]
    struct HostCheckFixture {
        _temp: tempfile::TempDir,
        data_paths: DataPaths,
        package: NativePackageSelection,
        policy_path: PathBuf,
        manifest: serde_json::Value,
    }

    impl HostCheckFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let resources = temp.path().join("resources");
            let package_root = resources.join("coding-agent");
            let executor = package_root.join("bin/coding-agent-executor");
            let runtime = package_root.join("runtime/all-in-one");
            private_dir(&resources);
            private_dir(&package_root);
            private_dir(&package_root.join("bin"));
            private_dir(&package_root.join("runtime"));
            write_private_file(&executor, b"test executor");
            fs::set_permissions(&executor, fs::Permissions::from_mode(0o700)).unwrap();
            write_private_file(&runtime, b"test runtime");

            let manifest = json!({
                "schema_version": 1,
                "profile": { "name": "linux-kvm", "version": 1 },
                "target": { "triple": TEST_TARGET_TRIPLE },
                "sources": {
                    "agent_app": {
                        "repository": "https://github.com/lookback/agent-app",
                        "ref": "release/native-terminal",
                        "revision": "a".repeat(40),
                        "dirty": false,
                        "submodules": []
                    },
                    "jobworkerp_runtime": {
                        "repository": "https://github.com/jobworkerp-rs/jobworkerp-rs",
                        "ref": "release/native-terminal",
                        "revision": "b".repeat(40),
                        "dirty": false,
                        "submodules": []
                    },
                    "jobworkerp_client": {
                        "repository": "https://github.com/jobworkerp-rs/jobworkerp-client-rs",
                        "revision": "c".repeat(40),
                        "lockfile_sha256": "d".repeat(64)
                    }
                },
                "executor": {
                    "version": EXECUTOR_VERSION,
                    "protocol_version": PROTOCOL_VERSION,
                    "capabilities": ["check_repository", "prepare"]
                },
                "runtime_compatibility": {
                    "protocol_version": null,
                    "config_version": null
                },
                "artifacts": {
                    "runtime": {
                        "path": "runtime/all-in-one",
                        "sha256": digest(b"test runtime")
                    },
                    "executor": {
                        "path": "bin/coding-agent-executor",
                        "sha256": digest(b"test executor")
                    },
                    "configuration": []
                }
            });
            write_private_file(
                &package_root.join("coding-agent-artifact-manifest.json"),
                serde_json::to_vec(&manifest).unwrap().as_slice(),
            );
            fs::set_permissions(
                package_root.join("coding-agent-artifact-manifest.json"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();

            let data_paths = DataPaths::with_root(temp.path().join("app-data"));
            private_dir(&data_paths.root);
            let package = NativePackageSelection::from_app_resources(&resources).unwrap();
            let fixture = Self {
                _temp: temp,
                policy_path: data_paths
                    .root
                    .join(crate::native_terminal::bootstrap::TRUSTED_HOST_POLICY_RELATIVE_PATH),
                data_paths,
                package,
                manifest: manifest.clone(),
            };
            fixture.write_policy(&manifest, &[repository_tuple("acme", "widget")]);
            fixture
        }

        fn write_policy(&self, manifest: &serde_json::Value, repositories: &[(&str, &str)]) {
            let policy_root = self
                .policy_path
                .parent()
                .expect("policy parent")
                .to_path_buf();
            private_dir(&policy_root);
            let policy = json!({
                "version": 1,
                "max_import_bytes": 16 * 1024 * 1024,
                "expected_package_identity": {
                    "profile": manifest["profile"],
                    "target": manifest["target"],
                    "sources": manifest["sources"],
                    "executor": manifest["executor"]
                },
                "repositories": repositories.iter().map(|(organization, name)| json!({
                    "id": { "host": "github.com", "owner": organization, "name": name },
                    "clone_url": format!("https://github.com/{organization}/{name}"),
                    "allowed_base_branches": ["main"],
                    "read_credential_source": "read",
                    "publish_credential_source": "publish",
                    "forge": {
                        "kind": "github",
                        "api_base_url": "https://api.github.com",
                        "web_base_url": "https://github.com"
                    }
                })).collect::<Vec<_>>(),
                "credential_sources": [
                    { "id": "read", "username": "reader", "token_file": "read-token" },
                    { "id": "publish", "username": "publisher", "token_file": "publish-token" }
                ]
            });
            write_private_file(
                &self.policy_path,
                serde_json::to_vec(&policy).unwrap().as_slice(),
            );
        }

        fn write_git_only_policy(&self) {
            let mut policy: serde_json::Value =
                serde_json::from_slice(&fs::read(&self.policy_path).unwrap()).unwrap();
            for repository in policy["repositories"].as_array_mut().unwrap() {
                repository.as_object_mut().unwrap().remove("forge");
            }
            write_private_file(
                &self.policy_path,
                serde_json::to_vec(&policy).unwrap().as_slice(),
            );
        }

        fn settings(&self) -> CodingAgentSettingsStore {
            let store = CodingAgentSettingsStore::new(self.data_paths.coding_agent_settings_path());
            let settings = crate::coding_agent::settings::CodingAgentSettings {
                repository_hosts: vec![host()],
                ..Default::default()
            };
            let saved = store
                .compare_and_swap(0, settings, &std::collections::HashSet::new())
                .unwrap();
            store
                .compare_and_swap_native_baseline(
                    saved.revision,
                    self.package.package_fingerprint().to_owned(),
                )
                .unwrap();
            store
        }
    }

    fn repository_tuple<'a>(organization: &'a str, name: &'a str) -> (&'a str, &'a str) {
        (organization, name)
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    const TEST_TARGET_TRIPLE: &str = "x86_64-unknown-linux-gnu";
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    const TEST_TARGET_TRIPLE: &str = "aarch64-unknown-linux-gnu";
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "musl"))]
    const TEST_TARGET_TRIPLE: &str = "x86_64-unknown-linux-musl";
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "musl"))]
    const TEST_TARGET_TRIPLE: &str = "aarch64-unknown-linux-musl";

    fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn private_dir(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn write_private_file(path: &Path, bytes: &[u8]) {
        private_dir(path.parent().unwrap());
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true).mode(0o600);
        let mut file = options.open(path).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    struct TestRunner {
        calls: Arc<AtomicUsize>,
        observations: Arc<Mutex<Vec<(coding_agent_executor::RepositoryId, String)>>>,
        started: Mutex<Option<oneshot::Sender<()>>>,
        release: Mutex<Option<oneshot::Receiver<()>>>,
        result: Option<coding_agent_executor::RepositoryCheckResult>,
        fail_unexpectedly: bool,
    }

    impl TestRunner {
        fn successful(calls: Arc<AtomicUsize>) -> Self {
            Self {
                calls,
                observations: Arc::new(Mutex::new(Vec::new())),
                started: Mutex::new(None),
                release: Mutex::new(None),
                result: Some(executor_result(vec![
                    executor_stage("git_read", "passed", "git_readable"),
                    executor_stage("forge_api", "passed", "forge_readable"),
                ])),
                fail_unexpectedly: false,
            }
        }

        fn successful_git_only(calls: Arc<AtomicUsize>) -> Self {
            let mut runner = Self::successful(calls);
            runner.result = Some(executor_result(vec![
                executor_stage("git_read", "passed", "git_readable"),
                executor_stage("forge_api", "skipped", "forge_not_configured"),
            ]));
            runner
        }

        fn unexpected_error(calls: Arc<AtomicUsize>) -> Self {
            let mut runner = Self::successful(calls);
            runner.fail_unexpectedly = true;
            runner
        }

        fn gated(
            calls: Arc<AtomicUsize>,
            started: oneshot::Sender<()>,
            release: oneshot::Receiver<()>,
        ) -> Self {
            let mut runner = Self::successful(calls);
            runner.started = Mutex::new(Some(started));
            runner.release = Mutex::new(Some(release));
            runner
        }
    }

    impl RepositoryCheckRunner for TestRunner {
        fn run<'a>(
            &'a self,
            _data_paths: &'a DataPaths,
            _package: &'a NativePackageSelection,
            expected_policy_sha256: &'a str,
            repository: &'a coding_agent_executor::RepositoryId,
        ) -> RepositoryCheckFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.observations
                .lock()
                .unwrap()
                .push((repository.clone(), expected_policy_sha256.to_owned()));
            if let Some(started) = self.started.lock().unwrap().take() {
                let _ = started.send(());
            }
            let release = self.release.lock().unwrap().take();
            let result = self.result.clone();
            let fail_unexpectedly = self.fail_unexpectedly;
            Box::pin(async move {
                if let Some(release) = release {
                    let _ = release.await;
                }
                if fail_unexpectedly {
                    Err(NativeRepositoryCheckError::Executor(
                        NativeExecutorError::OutcomeUnknown {
                            operation: "token=/secret/path?url=https://attacker.invalid",
                        },
                    ))
                } else {
                    Ok(result.expect("successful test runner result"))
                }
            })
        }
    }

    #[test]
    fn structural_validation_accepts_a_bound_repository() {
        let repository = RepositoryIdentitySettings {
            host: "github.com".into(),
            organization: "acme".into(),
            repository: "widget".into(),
        };

        let validated = validate_request_shape(&host(), Some(&repository));

        assert!(validated.is_ok());
    }

    #[test]
    fn structural_validation_rejects_invalid_url_before_policy_or_io() {
        let mut invalid = host();
        invalid.api_base_url = "http://attacker.invalid".into();

        let error = validate_request_shape(&invalid, None).unwrap_err();

        assert_eq!(error, StructuralCheckError::InvalidSettings);
    }

    #[tokio::test]
    async fn service_rejects_invalid_host_before_optional_package_access() {
        let root = tempfile::tempdir().unwrap();
        let data_paths = DataPaths::with_root(root.path());
        let settings = CodingAgentSettingsStore::new(data_paths.coding_agent_settings_path());
        let mut invalid = host();
        invalid.api_base_url = "http://attacker.invalid".into();
        let response = check_repository_host(
            &data_paths,
            &settings,
            None,
            None,
            RepositoryHostCheckRequest {
                expected_revision: 0,
                host: invalid,
                repository: Some(RepositoryIdentitySettings {
                    host: "github.com".into(),
                    organization: "acme".into(),
                    repository: "widget".into(),
                }),
            },
        )
        .await
        .unwrap();

        assert_eq!(response.stages[0].code, CheckCode::InvalidSettings);
        assert!(
            response
                .stages
                .iter()
                .skip(1)
                .all(|stage| stage.code == CheckCode::PrerequisiteFailed)
        );
    }

    #[test]
    fn missing_repository_produces_fixed_prerequisite_stages() {
        let stages = stages_for_missing_repository(17);

        assert_eq!(stages.len(), 4);
        assert_eq!(stages[0].stage, CheckStageName::Configuration);
        assert_eq!(stages[0].status, CheckStageStatus::Passed);
        assert_eq!(stages[0].code, CheckCode::SettingsValid);
        assert_eq!(stages[1].status, CheckStageStatus::Skipped);
        assert_eq!(stages[1].code, CheckCode::RepositoryRequired);
        assert!(
            stages[2..]
                .iter()
                .all(|stage| stage.status == CheckStageStatus::Skipped
                    && stage.code == CheckCode::PrerequisiteFailed)
        );
    }

    #[test]
    fn response_wire_shape_matches_the_renderer_contract() {
        let repository = RepositoryIdentitySettings {
            host: "github.com".into(),
            organization: "acme".into(),
            repository: "widget".into(),
        };
        let value = serde_json::to_value(response(
            17,
            Some(repository),
            vec![stage(
                CheckStageName::TrustedPolicy,
                CheckStageStatus::Passed,
                CheckCode::PolicyMatched,
            )],
        ))
        .unwrap();

        assert_eq!(value["settingsRevision"], 17);
        assert_eq!(value["repository"]["organization"], "acme");
        assert_eq!(value["stages"][0]["stage"], "trusted_policy");
        assert_eq!(value["stages"][0]["status"], "passed");
        assert_eq!(value["stages"][0]["code"], "policy_matched");
        assert!(value.get("settings_revision").is_none());
    }

    #[test]
    fn executor_forge_unavailable_preserves_stage_order_and_git_result() {
        let result = executor_result(vec![
            executor_stage("git_read", "passed", "git_readable"),
            executor_stage("forge_api", "failed", "unavailable"),
        ]);

        let stages = result_stages(result);

        assert_eq!(
            stages,
            vec![
                stage(
                    CheckStageName::GitRead,
                    CheckStageStatus::Passed,
                    CheckCode::GitReadable,
                ),
                stage(
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Failed,
                    CheckCode::Unavailable,
                ),
            ]
        );
    }

    #[tokio::test]
    async fn missing_repository_only_validates_shape_without_policy_or_runner() {
        let temp = tempfile::tempdir().unwrap();
        let data_paths = DataPaths::with_root(temp.path());
        let settings = CodingAgentSettingsStore::new(data_paths.coding_agent_settings_path());
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = TestRunner::successful(Arc::clone(&calls));

        let response = check_repository_host_with_runner(
            &data_paths,
            &settings,
            None,
            None,
            RepositoryHostCheckRequest {
                expected_revision: 0,
                ..request(None)
            },
            &runner,
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(response.stages, stages_for_missing_repository(0));
    }

    #[tokio::test]
    async fn policy_failures_are_stable_and_never_start_the_runner() {
        let cases = [
            ("missing", CheckCode::RepositoryNotApproved),
            ("invalid", CheckCode::PolicyInvalid),
            ("unapproved", CheckCode::RepositoryNotApproved),
        ];
        for (kind, expected_code) in cases {
            let fixture = HostCheckFixture::new();
            if kind == "missing" {
                fs::remove_file(&fixture.policy_path).unwrap();
            } else if kind == "invalid" {
                write_private_file(&fixture.policy_path, b"not-json");
            } else {
                fixture.write_policy(&fixture.manifest, &[("other", "widget")]);
            }
            let settings = fixture.settings();
            let calls = Arc::new(AtomicUsize::new(0));
            let runner = TestRunner::successful(Arc::clone(&calls));
            let response = check_repository_host_with_runner(
                &fixture.data_paths,
                &settings,
                Some(&fixture.package),
                None,
                request(Some(repository())),
                &runner,
            )
            .await
            .unwrap();

            assert_eq!(calls.load(Ordering::SeqCst), 0, "case: {kind}");
            assert_eq!(response.stages[1].code, expected_code, "case: {kind}");
            assert!(
                response.stages[2..]
                    .iter()
                    .all(|stage| stage.code == CheckCode::PrerequisiteFailed),
                "case: {kind}"
            );
        }
    }

    #[tokio::test]
    async fn every_policy_binding_mismatch_rejects_before_credentials_or_runner() {
        let cases = ["clone", "forge", "api", "read", "publication"];
        for mismatch in cases {
            let fixture = HostCheckFixture::new();
            let mut requested_host = host();
            match mismatch {
                "clone" => {
                    requested_host.clone_transport = CloneTransportSettings::Https {
                        base_url: "https://other.example".into(),
                    };
                }
                "forge" => requested_host.forge = crate::coding_agent::ForgeKind::Gitea,
                "api" => requested_host.api_base_url = "https://api.other.example".into(),
                "read" => requested_host.read_credential_ref = Some("other-read".into()),
                "publication" => {
                    requested_host.publication_credential_ref = Some("other-publish".into());
                }
                _ => unreachable!(),
            }
            let settings = fixture.settings();
            let calls = Arc::new(AtomicUsize::new(0));
            let runner = TestRunner::successful(Arc::clone(&calls));
            let response = check_repository_host_with_runner(
                &fixture.data_paths,
                &settings,
                Some(&fixture.package),
                None,
                RepositoryHostCheckRequest {
                    expected_revision: 2,
                    host: requested_host,
                    repository: Some(repository()),
                },
                &runner,
            )
            .await
            .unwrap();

            assert_eq!(calls.load(Ordering::SeqCst), 0, "mismatch: {mismatch}");
            assert_eq!(response.stages[1].code, CheckCode::PolicyMismatch);
        }
    }

    #[tokio::test]
    async fn precheck_leaves_artifact_rehashing_to_the_executing_runner() {
        let fixture = HostCheckFixture::new();
        let settings = fixture.settings();
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = TestRunner::successful(Arc::clone(&calls));
        let expected_digest = crate::native_terminal::bootstrap::read_repository_policy(
            &fixture.data_paths,
            &fixture.package,
        )
        .unwrap()
        .policy_sha256;
        // Same length, different bytes. The production runner
        // (`execute_repository_check`) re-hashes artifacts before executing.
        fs::write(
            fixture
                ._temp
                .path()
                .join("resources/coding-agent/runtime/all-in-one"),
            b"TEST RUNTIME",
        )
        .unwrap();

        check_repository_host_with_runner(
            &fixture.data_paths,
            &settings,
            Some(&fixture.package),
            None,
            request(Some(repository())),
            &runner,
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(runner.observations.lock().unwrap()[0].1, expected_digest);
    }

    #[tokio::test]
    async fn matching_policy_passes_repository_and_digest_to_runner_and_maps_four_stages() {
        let fixture = HostCheckFixture::new();
        let settings = fixture.settings();
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = TestRunner::successful(Arc::clone(&calls));
        let expected_digest = crate::native_terminal::bootstrap::read_repository_policy(
            &fixture.data_paths,
            &fixture.package,
        )
        .unwrap()
        .policy_sha256;

        let response = check_repository_host_with_runner(
            &fixture.data_paths,
            &settings,
            Some(&fixture.package),
            None,
            request(Some(repository())),
            &runner,
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            runner.observations.lock().unwrap().as_slice(),
            &[(
                coding_agent_executor::RepositoryId {
                    host: "github.com".into(),
                    owner: "acme".into(),
                    name: "widget".into(),
                },
                expected_digest,
            )]
        );
        assert_eq!(
            response.stages,
            vec![
                stage(
                    CheckStageName::Configuration,
                    CheckStageStatus::Passed,
                    CheckCode::SettingsValid,
                ),
                stage(
                    CheckStageName::TrustedPolicy,
                    CheckStageStatus::Passed,
                    CheckCode::PolicyMatched,
                ),
                stage(
                    CheckStageName::GitRead,
                    CheckStageStatus::Passed,
                    CheckCode::GitReadable,
                ),
                stage(
                    CheckStageName::ForgeApi,
                    CheckStageStatus::Passed,
                    CheckCode::ForgeReadable,
                ),
            ]
        );
    }

    #[tokio::test]
    async fn legacy_git_only_policy_allows_git_read_and_skips_forge_api() {
        let fixture = HostCheckFixture::new();
        fixture.write_git_only_policy();
        let settings = fixture.settings();
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = TestRunner::successful_git_only(Arc::clone(&calls));

        let response = check_repository_host_with_runner(
            &fixture.data_paths,
            &settings,
            Some(&fixture.package),
            None,
            request(Some(repository())),
            &runner,
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(response.stages[1].code, CheckCode::PolicyMatched);
        assert_eq!(response.stages[2].code, CheckCode::GitReadable);
        assert_eq!(response.stages[3].code, CheckCode::ForgeNotConfigured);
        assert_eq!(response.stages[3].status, CheckStageStatus::Skipped);
    }

    #[tokio::test]
    async fn repository_check_never_runs_before_explicit_package_approval() {
        let fixture = HostCheckFixture::new();
        let settings =
            CodingAgentSettingsStore::new(fixture.data_paths.root.join("unapproved-settings.json"));
        settings
            .compare_and_swap(
                0,
                CodingAgentSettings {
                    repository_hosts: vec![host()],
                    ..Default::default()
                },
                &std::collections::HashSet::new(),
            )
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = TestRunner::successful(Arc::clone(&calls));
        let response = check_repository_host_with_runner(
            &fixture.data_paths,
            &settings,
            Some(&fixture.package),
            None,
            RepositoryHostCheckRequest {
                expected_revision: 1,
                ..request(Some(repository()))
            },
            &runner,
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(response.stages[1].code, CheckCode::NativeBackendNotApproved);
    }

    #[tokio::test]
    async fn null_host_credentials_inherit_fixed_policy_refs_for_gui_branch_approval() {
        let fixture = HostCheckFixture::new();
        let settings = fixture.settings();
        let mut proposed = settings.load().unwrap();
        proposed.repository_hosts[0].read_credential_ref = None;
        proposed.repository_hosts[0].publication_credential_ref = None;
        proposed.native_repository_approvals = Some(vec![
            crate::coding_agent::settings::NativeRepositoryApprovalSettings {
                repository: repository(),
                allowed_base_branches: vec!["main".into()],
            },
        ]);
        settings
            .compare_and_swap(2, proposed, &std::collections::HashSet::new())
            .unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let runner = TestRunner::successful(Arc::clone(&calls));
        let mut requested_host = host();
        requested_host.read_credential_ref = None;
        requested_host.publication_credential_ref = None;
        let response = check_repository_host_with_runner(
            &fixture.data_paths,
            &settings,
            Some(&fixture.package),
            None,
            RepositoryHostCheckRequest {
                expected_revision: 3,
                host: requested_host,
                repository: Some(repository()),
            },
            &runner,
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(response.stages[1].code, CheckCode::PolicyMatched);
    }

    #[tokio::test]
    async fn explicit_wrong_credential_ref_still_blocks_gui_branch_approval_before_runner() {
        let fixture = HostCheckFixture::new();
        let settings = fixture.settings();
        let mut proposed = settings.load().unwrap();
        proposed.repository_hosts[0].read_credential_ref = None;
        proposed.repository_hosts[0].publication_credential_ref = None;
        proposed.native_repository_approvals = Some(vec![
            crate::coding_agent::settings::NativeRepositoryApprovalSettings {
                repository: repository(),
                allowed_base_branches: vec!["main".into()],
            },
        ]);
        settings
            .compare_and_swap(2, proposed, &std::collections::HashSet::new())
            .unwrap();

        let mut requested_host = host();
        requested_host.read_credential_ref = Some("wrong-read".into());
        requested_host.publication_credential_ref = None;
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = TestRunner::successful(Arc::clone(&calls));
        let response = check_repository_host_with_runner(
            &fixture.data_paths,
            &settings,
            Some(&fixture.package),
            None,
            RepositoryHostCheckRequest {
                expected_revision: 3,
                host: requested_host,
                repository: Some(repository()),
            },
            &runner,
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(response.stages[1].code, CheckCode::PolicyMismatch);
    }

    #[tokio::test]
    async fn settings_saved_while_runner_waits_invalidates_the_result_by_revision() {
        let fixture = HostCheckFixture::new();
        let data_paths = Arc::new(fixture.data_paths.clone());
        let settings = Arc::new(fixture.settings());
        let package = Arc::new(fixture.package.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let runner = Arc::new(TestRunner::gated(
            Arc::clone(&calls),
            started_tx,
            release_rx,
        ));
        let check_data_paths = Arc::clone(&data_paths);
        let check_settings = Arc::clone(&settings);
        let check_package = Arc::clone(&package);
        let check_runner = Arc::clone(&runner);
        let check = tokio::spawn(async move {
            check_repository_host_with_runner(
                check_data_paths.as_ref(),
                check_settings.as_ref(),
                Some(check_package.as_ref()),
                None,
                request(Some(repository())),
                check_runner.as_ref(),
            )
            .await
        });

        started_rx.await.unwrap();
        let save_settings = Arc::clone(&settings);
        let save = tokio::task::spawn_blocking(move || {
            let proposed = save_settings.load().unwrap();
            save_settings
                .compare_and_swap(2, proposed, &std::collections::HashSet::new())
                .unwrap()
        });
        let save_result = tokio::time::timeout(Duration::from_secs(1), save).await;
        release_tx.send(()).unwrap();
        let response = check.await.unwrap().unwrap();
        let saved = save_result
            .expect("settings save blocked while runner was waiting")
            .expect("settings save task failed");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(saved.revision, 3);
        assert_eq!(response.settings_revision, 3);
        assert_eq!(response.stages[0].code, CheckCode::RevisionConflict);
        assert!(
            response.stages[1..]
                .iter()
                .all(|stage| stage.code == CheckCode::PrerequisiteFailed)
        );
    }

    #[tokio::test]
    async fn unexpected_runner_errors_map_to_stable_codes_without_raw_details() {
        let fixture = HostCheckFixture::new();
        let settings = fixture.settings();
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = TestRunner::unexpected_error(Arc::clone(&calls));

        let response = check_repository_host_with_runner(
            &fixture.data_paths,
            &settings,
            Some(&fixture.package),
            None,
            request(Some(repository())),
            &runner,
        )
        .await
        .unwrap();
        let rendered = serde_json::to_string(&response).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(response.stages[2].code, CheckCode::Unavailable);
        assert_eq!(response.stages[3].code, CheckCode::PrerequisiteFailed);
        assert!(!rendered.contains("/secret/path"));
        assert!(!rendered.contains("attacker.invalid"));
        assert!(!rendered.contains("token="));
    }

    #[test]
    fn every_client_accepted_result_tuple_keeps_its_stage() {
        let git_cases = [
            (
                "passed",
                "git_readable",
                CheckStageStatus::Passed,
                CheckCode::GitReadable,
            ),
            (
                "failed",
                "git_failed",
                CheckStageStatus::Failed,
                CheckCode::GitFailed,
            ),
            (
                "failed",
                "credential_unavailable",
                CheckStageStatus::Failed,
                CheckCode::CredentialUnavailable,
            ),
            (
                "failed",
                "timeout",
                CheckStageStatus::Failed,
                CheckCode::Timeout,
            ),
            (
                "failed",
                "response_too_large",
                CheckStageStatus::Failed,
                CheckCode::ResponseTooLarge,
            ),
            (
                "failed",
                "unavailable",
                CheckStageStatus::Failed,
                CheckCode::Unavailable,
            ),
        ];
        for (status, code, expected_status, expected_code) in git_cases {
            let stages = result_stages(executor_result(vec![
                executor_stage("git_read", status, code),
                executor_stage("forge_api", "skipped", "forge_not_configured"),
            ]));

            assert_eq!(stages[0].stage, CheckStageName::GitRead);
            assert_eq!(stages[0].status, expected_status);
            assert_eq!(stages[0].code, expected_code);
            assert_eq!(stages[1].stage, CheckStageName::ForgeApi);
        }

        let forge_cases = [
            (
                "passed",
                "forge_readable",
                CheckStageStatus::Passed,
                CheckCode::ForgeReadable,
            ),
            (
                "failed",
                "forge_not_configured",
                CheckStageStatus::Failed,
                CheckCode::ForgeNotConfigured,
            ),
            (
                "skipped",
                "forge_not_configured",
                CheckStageStatus::Skipped,
                CheckCode::ForgeNotConfigured,
            ),
            (
                "failed",
                "credential_not_configured",
                CheckStageStatus::Failed,
                CheckCode::CredentialNotConfigured,
            ),
            (
                "skipped",
                "credential_not_configured",
                CheckStageStatus::Skipped,
                CheckCode::CredentialNotConfigured,
            ),
            (
                "failed",
                "credential_unavailable",
                CheckStageStatus::Failed,
                CheckCode::CredentialUnavailable,
            ),
            (
                "failed",
                "authentication_failed",
                CheckStageStatus::Failed,
                CheckCode::AuthenticationFailed,
            ),
            (
                "failed",
                "access_denied",
                CheckStageStatus::Failed,
                CheckCode::AccessDenied,
            ),
            (
                "failed",
                "repository_not_found",
                CheckStageStatus::Failed,
                CheckCode::RepositoryNotFound,
            ),
            (
                "failed",
                "redirect_rejected",
                CheckStageStatus::Failed,
                CheckCode::RedirectRejected,
            ),
            (
                "failed",
                "invalid_response",
                CheckStageStatus::Failed,
                CheckCode::InvalidResponse,
            ),
            (
                "failed",
                "timeout",
                CheckStageStatus::Failed,
                CheckCode::Timeout,
            ),
            (
                "failed",
                "response_too_large",
                CheckStageStatus::Failed,
                CheckCode::ResponseTooLarge,
            ),
            (
                "failed",
                "unavailable",
                CheckStageStatus::Failed,
                CheckCode::Unavailable,
            ),
        ];
        for (status, code, expected_status, expected_code) in forge_cases {
            let stages = result_stages(executor_result(vec![
                executor_stage("git_read", "passed", "git_readable"),
                executor_stage("forge_api", status, code),
            ]));

            assert_eq!(stages[0].stage, CheckStageName::GitRead);
            assert_eq!(stages[1].stage, CheckStageName::ForgeApi);
            assert_eq!(stages[1].status, expected_status);
            assert_eq!(stages[1].code, expected_code);
        }
    }

    #[test]
    fn diagnostic_transport_failures_keep_stable_common_codes() {
        for (reason, code) in [
            (NativeDiagnosticTransportReason::Timeout, CheckCode::Timeout),
            (
                NativeDiagnosticTransportReason::ResponseTooLarge,
                CheckCode::ResponseTooLarge,
            ),
            (
                NativeDiagnosticTransportReason::Unavailable,
                CheckCode::Unavailable,
            ),
        ] {
            let stages = failed_executor_stages(NativeRepositoryCheckError::Executor(
                NativeExecutorError::DiagnosticTransport { reason },
            ));

            assert_eq!(stages[0].stage, CheckStageName::GitRead);
            assert_eq!(stages[0].status, CheckStageStatus::Failed);
            assert_eq!(stages[0].code, code);
            assert_eq!(stages[1].stage, CheckStageName::ForgeApi);
            assert_eq!(stages[1].status, CheckStageStatus::Skipped);
        }
    }

    fn executor_result(
        stages: Vec<coding_agent_executor::RepositoryCheckStage>,
    ) -> coding_agent_executor::RepositoryCheckResult {
        coding_agent_executor::RepositoryCheckResult {
            repository: coding_agent_executor::RepositoryId {
                host: "github.com".into(),
                owner: "acme".into(),
                name: "widget".into(),
            },
            trusted_config_digest: "a".repeat(64),
            protocol_version: coding_agent_executor::PROTOCOL_VERSION,
            executor_version: coding_agent_executor::EXECUTOR_VERSION.into(),
            stages,
        }
    }

    fn executor_stage(
        stage: &str,
        status: &str,
        code: &str,
    ) -> coding_agent_executor::RepositoryCheckStage {
        coding_agent_executor::RepositoryCheckStage {
            stage: stage.into(),
            status: status.into(),
            code: code.into(),
        }
    }
}
