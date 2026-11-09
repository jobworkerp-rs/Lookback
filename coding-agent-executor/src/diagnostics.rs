use std::ffi::OsString;
use std::time::Duration;

use reqwest::StatusCode;
use serde_json::Value;

use crate::ValidatedConfig;
use crate::git_context::{GitCheckFailure, GitContext};
use crate::model::{
    CheckRepositoryChecks, RepositoryCheckResult, RepositoryCheckStage, RepositoryConfig,
    RepositoryId,
};
use crate::publication::{
    ForgeResponseReadError, forge_api_url, forge_authorization, forge_client, forge_repository_url,
    read_limited_response_reason,
};

const FORGE_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const FORGE_CHECK_RESPONSE_BYTES: usize = 64 * 1024;

pub(crate) async fn check_repository(
    config: &ValidatedConfig,
    repository: &RepositoryConfig,
    checks: Option<&CheckRepositoryChecks>,
) -> RepositoryCheckResult {
    let checks = checks.cloned().unwrap_or(CheckRepositoryChecks {
        git_read: true,
        forge_api: true,
    });
    let git_config = config.clone();
    let git_repository = repository.clone();
    let git_stage = if checks.git_read {
        tokio::task::spawn_blocking(move || check_git(&git_config, &git_repository))
            .await
            .unwrap_or_else(|_| stage("git_read", "failed", "unavailable"))
    } else {
        stage("git_read", "skipped", "prerequisite_failed")
    };
    let forge_stage = if checks.forge_api {
        check_forge(config, repository).await
    } else {
        stage("forge_api", "skipped", "prerequisite_failed")
    };
    RepositoryCheckResult {
        repository: repository.id.clone(),
        trusted_config_digest: config.trusted_config_digest.clone(),
        protocol_version: crate::PROTOCOL_VERSION,
        executor_version: crate::EXECUTOR_VERSION.to_owned(),
        stages: vec![git_stage, forge_stage],
    }
}

fn check_git(config: &ValidatedConfig, repository: &RepositoryConfig) -> RepositoryCheckStage {
    let git = match GitContext::for_repository(&config.state_root, repository) {
        Ok(git) => git,
        Err(_) => return stage("git_read", "failed", "git_failed"),
    };
    let credential = match repository
        .read_credential_source
        .as_deref()
        .map(|id| config.credential_source(id))
        .transpose()
    {
        Ok(credential) => credential,
        Err(_) => return stage("git_read", "failed", "credential_unavailable"),
    };
    let args = vec![
        OsString::from("ls-remote"),
        OsString::from("--symref"),
        OsString::from("--"),
        repository.clone_url.clone().into(),
        OsString::from("HEAD"),
    ];
    match git.check_remote(&config.state_root, &args, credential.as_ref()) {
        Ok(output) if output.success => stage("git_read", "passed", "git_readable"),
        Ok(_) => stage("git_read", "failed", "git_failed"),
        Err(GitCheckFailure::Timeout) => stage("git_read", "failed", "timeout"),
        Err(GitCheckFailure::ResponseTooLarge) => stage("git_read", "failed", "response_too_large"),
        Err(GitCheckFailure::Failed) => stage("git_read", "failed", "git_failed"),
    }
}

async fn check_forge(
    config: &ValidatedConfig,
    repository: &RepositoryConfig,
) -> RepositoryCheckStage {
    let Some(forge) = repository.forge.as_ref() else {
        return stage("forge_api", "skipped", "forge_not_configured");
    };
    let Some(credential_id) = repository.publish_credential_source.as_deref() else {
        return stage("forge_api", "skipped", "credential_not_configured");
    };
    let (_, token) = match config.read_credential(credential_id) {
        Ok(credential) => credential,
        Err(_) => return stage("forge_api", "failed", "credential_unavailable"),
    };
    let client = match forge_client(FORGE_CHECK_TIMEOUT) {
        Ok(client) => client,
        Err(_) => return stage("forge_api", "failed", "unavailable"),
    };
    let authorization = match forge_authorization(forge.kind, &token) {
        Ok(authorization) => authorization,
        Err(_) => return stage("forge_api", "failed", "credential_unavailable"),
    };
    let user_url = match forge_api_url(forge, &["user"]) {
        Ok(url) => url,
        Err(_) => return stage("forge_api", "failed", "invalid_response"),
    };
    if let Err(code) = get_json(&client, user_url, authorization.clone(), true).await {
        return stage("forge_api", "failed", code);
    }
    let repository_url = match forge_repository_url(forge, &repository.id, &[]) {
        Ok(url) => url,
        Err(_) => return stage("forge_api", "failed", "invalid_response"),
    };
    match get_json(&client, repository_url, authorization, false).await {
        Ok(body) if valid_repository_response(&body, &repository.id) => {
            stage("forge_api", "passed", "forge_readable")
        }
        Ok(_) => stage("forge_api", "failed", "invalid_response"),
        Err(code) => stage("forge_api", "failed", code),
    }
}

async fn get_json(
    client: &reqwest::Client,
    url: url::Url,
    authorization: reqwest::header::HeaderValue,
    user_request: bool,
) -> Result<Value, &'static str> {
    let response = client
        .get(url)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                "timeout"
            } else {
                "unavailable"
            }
        })?;
    let status = response.status();
    if status.is_redirection() {
        return Err("redirect_rejected");
    }
    if status == StatusCode::UNAUTHORIZED {
        return Err("authentication_failed");
    }
    if status == StatusCode::FORBIDDEN {
        return Err("access_denied");
    }
    if status == StatusCode::NOT_FOUND {
        return Err("repository_not_found");
    }
    if status != StatusCode::OK {
        return Err("unavailable");
    }
    let body = read_limited_response_reason(response, FORGE_CHECK_RESPONSE_BYTES)
        .await
        .map_err(|error| match error {
            ForgeResponseReadError::ResponseTooLarge => "response_too_large",
            ForgeResponseReadError::Timeout => "timeout",
            ForgeResponseReadError::Unavailable => "unavailable",
        })?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| "invalid_response")?;
    if !valid_user_response(&value) && user_request {
        return Err("invalid_response");
    }
    Ok(value)
}

fn valid_user_response(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    ["login", "username"].into_iter().any(|key| {
        object
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty() && value.len() <= 255)
    })
}

fn valid_repository_response(value: &Value, repository: &RepositoryId) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.get("name").and_then(Value::as_str) != Some(repository.name.as_str()) {
        return false;
    }
    let expected_full_name = format!("{}/{}", repository.owner, repository.name);
    let full_name_matches = object
        .get("full_name")
        .and_then(Value::as_str)
        .is_some_and(|value| value == expected_full_name);
    let owner_matches = object
        .get("owner")
        .and_then(Value::as_object)
        .and_then(|owner| {
            ["login", "username"]
                .into_iter()
                .find_map(|key| owner.get(key).and_then(Value::as_str))
        })
        .is_some_and(|value| value == repository.owner);
    full_name_matches || owner_matches
}

fn stage(stage: &str, status: &str, code: &str) -> RepositoryCheckStage {
    RepositoryCheckStage {
        stage: stage.to_owned(),
        status: status.to_owned(),
        code: code.to_owned(),
    }
}
