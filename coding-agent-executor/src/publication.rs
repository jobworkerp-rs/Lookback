use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::ValidatedConfig;
use crate::artifact::require_push_artifact;
use crate::git_context::{GitContext, strings};
use crate::model::{
    CreatePullRequestRequest, ExecutorError, ExecutorErrorCode, ForgeConfig, ForgeKind,
    PullRequestReceipt, PushReceipt, PushRequest, RepositoryConfig, SecretScanStatus,
};
use crate::secret_scan::{self, scan_frozen_history};
use crate::workspace::{
    acquire_run_lock, fetch_base_branch, prepared_metadata_record, read_json, verify_commit_type,
    write_json_atomic,
};

const MAX_FORGE_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_PULL_REQUEST_PAGES: usize = 100;
const MAX_PULL_REQUEST_SCAN_BYTES: usize = 16 * 1024 * 1024;
const PULL_REQUEST_PAGE_SIZE: usize = 100;
const RUN_MARKER_PREFIX: &str = "<!-- coding-agent-run:";

#[cfg(test)]
mod new_branch_tests {
    use super::*;

    #[test]
    fn newly_created_remote_branch_cannot_be_fast_forwarded_by_new_branch_publication() {
        let temp = tempfile::tempdir().unwrap();
        let git = GitContext::new(temp.path()).unwrap();
        let source = temp.path().join("source");
        let remote = temp.path().join("remote.git");
        fs::create_dir(&source).unwrap();
        git.checked(
            temp.path(),
            &strings(&["init", "--bare", remote.to_str().unwrap()]),
            None,
        )
        .unwrap();
        git.checked(&source, &strings(&["init", "--initial-branch=main"]), None)
            .unwrap();
        let commit = |message: &str| {
            git.checked(
                &source,
                &strings(&[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-m",
                    message,
                ]),
                None,
            )
            .unwrap();
            String::from_utf8(
                git.checked(&source, &strings(&["rev-parse", "HEAD"]), None)
                    .unwrap(),
            )
            .unwrap()
            .trim()
            .to_owned()
        };
        let base = commit("base");
        git.checked(
            &source,
            &strings(&["remote", "add", "origin", remote.to_str().unwrap()]),
            None,
        )
        .unwrap();
        git.checked(
            &source,
            &strings(&["push", "origin", "HEAD:refs/heads/agent/concurrent"]),
            None,
        )
        .unwrap();
        let head = commit("result");
        assert!(!push_new_branch(&git, &source, "agent/concurrent", &head, None).unwrap());
        let actual = git
            .checked(
                &remote,
                &strings(&["rev-parse", "refs/heads/agent/concurrent"]),
                None,
            )
            .unwrap();
        assert_eq!(String::from_utf8(actual).unwrap().trim(), base);
        assert!(push_new_branch(&git, &source, "agent/new", &head, None).unwrap());
        let actual = git
            .checked(
                &remote,
                &strings(&["rev-parse", "refs/heads/agent/new"]),
                None,
            )
            .unwrap();
        assert_eq!(String::from_utf8(actual).unwrap().trim(), head);
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PushRecord {
    request: PushRequest,
    receipt: PushReceipt,
}

fn push_new_branch(
    git: &GitContext,
    publisher: &Path,
    branch: &str,
    commit: &str,
    credential: Option<&crate::ResolvedCredentialSource>,
) -> Result<bool, ExecutorError> {
    let refspec = format!("{commit}:refs/heads/{branch}");
    // An empty expected value permits creation only; it never replaces an existing ref.
    let creation_lease = format!("--force-with-lease=refs/heads/{branch}:");
    git.run_silent(
        publisher,
        &strings(&[
            "push",
            "--no-verify",
            "--porcelain",
            &creation_lease,
            "origin",
            &refspec,
        ]),
        credential,
    )
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PullRequestRecord {
    request_hash: String,
    receipt: Option<PullRequestReceipt>,
}

pub(crate) fn push(
    config: &ValidatedConfig,
    request: &PushRequest,
) -> Result<PushReceipt, ExecutorError> {
    let _lock = acquire_run_lock(config, &request.run_id)?;
    push_locked(config, request, None)
}

pub(crate) fn lookup_push(
    config: &ValidatedConfig,
    request: &PushRequest,
) -> Result<PushReceipt, ExecutorError> {
    let _lock = acquire_run_lock(config, &request.run_id)?;
    lookup_push_locked(config, request)
}

fn lookup_push_locked(
    config: &ValidatedConfig,
    request: &PushRequest,
) -> Result<PushReceipt, ExecutorError> {
    let repository = config.repository(&request.repository)?;
    let prepared = prepared_metadata_record(
        config,
        &request.run_id,
        &request.attempt_id,
        &request.repository,
        &request.base_branch,
        &request.base_commit,
    )?;
    if request.branch != prepared.result.branch {
        return Err(ExecutorError::new(ExecutorErrorCode::BranchNotApproved));
    }
    let publisher = config.publisher_root.join(&request.run_id);
    let git = GitContext::for_repository(&config.state_root, repository)?;
    verify_existing_publisher(&git, repository, &publisher)?;
    let receipt_path = config
        .state_root
        .join("runs")
        .join(&request.run_id)
        .join("pushed.json");
    if read_json::<PushRecord>(&receipt_path)?.is_some_and(|record| record.request != *request) {
        return Err(ExecutorError::new(ExecutorErrorCode::IdempotencyConflict));
    }
    verify_commit_type(&git, &publisher, &request.frozen_commit)?;
    git.checked(
        &publisher,
        &strings(&[
            "merge-base",
            "--is-ancestor",
            &request.base_commit,
            &request.frozen_commit,
        ]),
        None,
    )?;
    let known_secrets = config.known_secrets(repository)?;
    scan_frozen_history(
        &git,
        &publisher,
        &request.base_commit,
        &request.frozen_commit,
        &known_secrets,
    )?;
    let credential = repository
        .read_credential_source
        .as_deref()
        .map(|id| config.credential_source(id))
        .transpose()?;
    if remote_branch_head(&git, &publisher, &request.branch, credential.as_ref())?.as_deref()
        != Some(request.frozen_commit.as_str())
    {
        return Err(ExecutorError::new(ExecutorErrorCode::RemoteBranchConflict));
    }
    let receipt = PushReceipt {
        run_id: request.run_id.clone(),
        attempt_id: request.attempt_id.clone(),
        repository: request.repository.clone(),
        branch: request.branch.clone(),
        commit: request.frozen_commit.clone(),
        remote_head: request.frozen_commit.clone(),
        artifact_digest: request.artifact_digest.clone(),
        already_present: true,
        secret_scan: SecretScanStatus::CleanSupportedFormatsV1,
    };
    // Only local recovery facts are saved; lookup never imports, fetches, or pushes.
    write_json_atomic(
        &receipt_path,
        &PushRecord {
            request: request.clone(),
            receipt: receipt.clone(),
        },
    )?;
    Ok(receipt)
}

pub(crate) fn push_workspace(
    config: &ValidatedConfig,
    request: &crate::WorkspacePushRequest,
) -> Result<PushReceipt, ExecutorError> {
    let push = PushRequest {
        run_id: request.source.run_id.clone(),
        attempt_id: request.source.attempt_id.clone(),
        repository: request.source.repository.clone(),
        branch: request.source.expected_branch(),
        base_branch: request.source.base_branch.clone(),
        base_commit: request.source.base_commit.clone(),
        // Preserve the receipt's content binding without claiming a seal.
        artifact_digest: request.source_digest.clone(),
        frozen_commit: request.expected_commit.clone(),
    };
    let _lock = acquire_run_lock(config, &push.run_id)?;
    push_locked(config, &push, Some(request))
}

fn push_locked(
    config: &ValidatedConfig,
    request: &PushRequest,
    workspace_request: Option<&crate::WorkspacePushRequest>,
) -> Result<PushReceipt, ExecutorError> {
    let repository = config.repository(&request.repository)?;
    let prepared = prepared_metadata_record(
        config,
        &request.run_id,
        &request.attempt_id,
        &request.repository,
        &request.base_branch,
        &request.base_commit,
    )?;
    if prepared.result.branch != prepared.request.expected_branch()
        || request.branch != prepared.result.branch
    {
        return Err(ExecutorError::new(ExecutorErrorCode::BranchNotApproved));
    }
    if let Some(workspace_request) = workspace_request {
        verify_workspace_source(config, workspace_request)?;
    } else {
        require_push_artifact(config, request)?;
    }

    let state_dir = config.state_root.join("runs").join(&request.run_id);
    let receipt_path = state_dir.join("pushed.json");
    let prior = read_json::<PushRecord>(&receipt_path)?;
    if prior
        .as_ref()
        .is_some_and(|prior| prior.request != *request)
    {
        return Err(ExecutorError::new(ExecutorErrorCode::IdempotencyConflict));
    }

    let publisher_path = ensure_publisher(config, repository, &request.run_id)?;
    if let Some(workspace_request) = workspace_request {
        crate::source::import_commit(
            config,
            &workspace_request.source,
            &request.frozen_commit,
            &publisher_path,
        )?;
        verify_workspace_source(config, workspace_request)?;
    }
    let git = GitContext::for_repository(&config.state_root, repository)?;
    let read_credential = repository
        .read_credential_source
        .as_deref()
        .map(|id| config.credential_source(id))
        .transpose()?;
    let publish_credential = repository
        .publish_credential_source
        .as_deref()
        .map(|id| config.credential_source(id))
        .transpose()?;

    fetch_base_branch(
        &git,
        &publisher_path,
        &request.base_branch,
        read_credential.as_ref(),
    )?;
    verify_commit_type(&git, &publisher_path, &request.frozen_commit)?;
    let fsck = git.run_silent(
        &publisher_path,
        &strings(&[
            "fsck",
            "--strict",
            "--no-reflogs",
            "--no-dangling",
            &request.frozen_commit,
        ]),
        None,
    )?;
    if !fsck {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    let object_format = git.checked(
        &publisher_path,
        &strings(&["rev-parse", "--show-object-format"]),
        None,
    )?;
    let object_format = std::str::from_utf8(&object_format)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?
        .trim();
    let oid_len = match object_format {
        "sha1" => 40,
        "sha256" => 64,
        _ => return Err(ExecutorError::new(ExecutorErrorCode::InvalidCommit)),
    };
    if request.frozen_commit.len() != oid_len {
        return Err(ExecutorError::new(ExecutorErrorCode::InvalidCommit));
    }
    let ancestor = git.run(
        &publisher_path,
        &strings(&[
            "merge-base",
            "--is-ancestor",
            &request.base_commit,
            &request.frozen_commit,
        ]),
        None,
    )?;
    if !ancestor.success {
        return Err(ExecutorError::new(
            ExecutorErrorCode::CommitNotBasedOnApprovedBase,
        ));
    }
    let unchanged = git.run(
        &publisher_path,
        &strings(&[
            "merge-base",
            "--is-ancestor",
            &request.frozen_commit,
            &request.base_commit,
        ]),
        None,
    )?;
    if unchanged.success || request.frozen_commit == request.base_commit {
        return Err(ExecutorError::new(ExecutorErrorCode::NoChangesToPublish));
    }
    let base_tree = commit_tree_id(&git, &publisher_path, &request.base_commit, oid_len)?;
    let frozen_tree = commit_tree_id(&git, &publisher_path, &request.frozen_commit, oid_len)?;
    if base_tree == frozen_tree {
        return Err(ExecutorError::new(ExecutorErrorCode::NoChangesToPublish));
    }
    let known_secrets = config.known_secrets(repository)?;
    scan_frozen_history(
        &git,
        &publisher_path,
        &request.base_commit,
        &request.frozen_commit,
        &known_secrets,
    )?;

    let remote_branch = remote_branch_head(
        &git,
        &publisher_path,
        &request.branch,
        read_credential.as_ref(),
    )?;
    if let Some(workspace_request) = workspace_request {
        verify_workspace_source(config, workspace_request)?;
    }
    if let Some(prior) = prior {
        if remote_branch.as_deref() != Some(request.frozen_commit.as_str()) {
            return Err(ExecutorError::new(ExecutorErrorCode::RemoteBranchConflict));
        }
        return Ok(prior.receipt);
    }

    let already_present = match remote_branch.as_deref() {
        Some(commit) if commit == request.frozen_commit => true,
        Some(_) => return Err(ExecutorError::new(ExecutorErrorCode::RemoteBranchConflict)),
        None => {
            let output = push_new_branch(
                &git,
                &publisher_path,
                &request.branch,
                &request.frozen_commit,
                publish_credential.as_ref(),
            )?;
            let after_push = remote_branch_head(
                &git,
                &publisher_path,
                &request.branch,
                read_credential.as_ref(),
            )?;
            if after_push.as_deref() == Some(request.frozen_commit.as_str()) {
                false
            } else if output || after_push.is_some() {
                return Err(ExecutorError::new(ExecutorErrorCode::RemoteBranchConflict));
            } else {
                return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
            }
        }
    };
    let receipt = PushReceipt {
        run_id: request.run_id.clone(),
        attempt_id: request.attempt_id.clone(),
        repository: request.repository.clone(),
        branch: request.branch.clone(),
        commit: request.frozen_commit.clone(),
        remote_head: request.frozen_commit.clone(),
        artifact_digest: request.artifact_digest.clone(),
        already_present,
        secret_scan: SecretScanStatus::CleanSupportedFormatsV1,
    };
    write_json_atomic(
        &receipt_path,
        &PushRecord {
            request: request.clone(),
            receipt: receipt.clone(),
        },
    )?;
    Ok(receipt)
}

fn verify_workspace_source(
    config: &ValidatedConfig,
    request: &crate::WorkspacePushRequest,
) -> Result<(), ExecutorError> {
    let observed = crate::source::inspect(config, &request.source)?;
    let bytes = serde_json::to_vec(&observed)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    if observed.head_commit != request.expected_commit
        || observed.branch != request.source.expected_branch()
        || !observed
            .uncommitted_diff
            .as_ref()
            .is_some_and(Vec::is_empty)
        || !observed.new_files.is_empty()
        || format!("{:x}", Sha256::digest(bytes)) != request.source_digest
    {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    Ok(())
}

fn commit_tree_id(
    git: &GitContext,
    repository: &Path,
    commit: &str,
    oid_len: usize,
) -> Result<String, ExecutorError> {
    let revision = format!("{commit}^{{tree}}");
    let output = git.checked(
        repository,
        &strings(&["rev-parse", "--verify", "--end-of-options", &revision]),
        None,
    )?;
    let tree = std::str::from_utf8(&output)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?
        .trim();
    if tree.len() != oid_len || !is_lower_hex(tree) {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    Ok(tree.to_owned())
}

pub(crate) async fn create_pull_request(
    config: &ValidatedConfig,
    request: &CreatePullRequestRequest,
    lookup_only: bool,
) -> Result<PullRequestReceipt, ExecutorError> {
    let lock_config = config.clone();
    let lock_run_id = request.run_id.clone();
    let _lock = tokio::task::spawn_blocking(move || acquire_run_lock(&lock_config, &lock_run_id))
        .await
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))??;
    let repository = config.repository(&request.repository)?;
    let state_dir = config.state_root.join("runs").join(&request.run_id);
    let push_record = read_json::<PushRecord>(&state_dir.join("pushed.json"))?
        .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::WorkspaceNotPrepared))?;
    if push_record.request.attempt_id != request.attempt_id
        || push_record.request.artifact_digest != request.artifact_digest
        || push_record.request.repository != request.repository
        || push_record.request.branch != request.branch
        || push_record.request.base_branch != request.base_branch
        || push_record.request.base_commit != request.base_commit
        || push_record.request.frozen_commit != request.frozen_commit
    {
        return Err(ExecutorError::new(ExecutorErrorCode::IdempotencyConflict));
    }
    let forge = repository
        .forge
        .as_ref()
        .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::PullRequestNotApproved))?;
    let marker = run_marker(&request.run_id);
    if request.pull_request.body.contains(RUN_MARKER_PREFIX) {
        return Err(ExecutorError::new(ExecutorErrorCode::InvalidRequest));
    }
    if contains_unapproved_close_directive(&request.pull_request.title)
        || contains_unapproved_close_directive(&request.pull_request.body)
    {
        return Err(ExecutorError::new(ExecutorErrorCode::InvalidRequest));
    }
    let publish_credential_id = repository
        .publish_credential_source
        .as_deref()
        .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::PullRequestNotApproved))?;
    let (username, token) = config.read_credential(publish_credential_id)?;
    let _ = username; // Forge API auth is a token; username is never sent or returned.
    let known_secrets = config.known_secrets(repository)?;
    let body = format!("{}\n\n{}", request.pull_request.body.trim_end(), marker);
    if !secret_scan::scan_bytes(request.pull_request.title.as_bytes(), &known_secrets).is_clean()
        || !secret_scan::scan_bytes(body.as_bytes(), &known_secrets).is_clean()
    {
        return Err(ExecutorError::new(ExecutorErrorCode::SecretDetected));
    }

    let request_hash = pull_request_hash(request)?;
    let receipt_path = state_dir.join("pull-request.json");
    let prior = read_json::<PullRequestRecord>(&receipt_path)?;
    if prior
        .as_ref()
        .is_some_and(|prior| prior.request_hash != request_hash)
    {
        return Err(ExecutorError::new(ExecutorErrorCode::IdempotencyConflict));
    }

    let remote_config = config.clone();
    let remote_run_id = request.run_id.clone();
    let remote_branch_name = request.branch.clone();
    let remote_repository = repository.clone();
    let read_credential_source = repository.read_credential_source.clone();
    let remote_head = tokio::task::spawn_blocking(move || {
        let git = GitContext::for_repository(&remote_config.state_root, &remote_repository)?;
        let publisher = remote_config.publisher_root.join(&remote_run_id);
        let read_credential = read_credential_source
            .as_deref()
            .map(|id| remote_config.credential_source(id))
            .transpose()?;
        remote_branch_head(
            &git,
            &publisher,
            &remote_branch_name,
            read_credential.as_ref(),
        )
    })
    .await
    .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))??;
    if remote_head.as_deref() != Some(request.frozen_commit.as_str()) {
        return Err(ExecutorError::new(ExecutorErrorCode::RemoteBranchConflict));
    }

    let client = forge_client(std::time::Duration::from_secs(25))?;
    let list_url = forge_repository_url(forge, &request.repository, &["pulls"])?;
    let existing = list_pull_requests(
        &client,
        &list_url,
        forge,
        &request.repository,
        &request.branch,
        &request.base_branch,
        &request.frozen_commit,
        &request.pull_request.title,
        &body,
        &marker,
        &token,
    )
    .await?;
    if let Some(mut receipt) = existing {
        receipt.run_id = request.run_id.clone();
        receipt.attempt_id = request.attempt_id.clone();
        receipt.repository = request.repository.clone();
        receipt.artifact_digest = request.artifact_digest.clone();
        receipt.already_present = true;
        if let Some(prior_receipt) = prior.as_ref().and_then(|prior| prior.receipt.as_ref()) {
            if prior_receipt.number != receipt.number || prior_receipt.url != receipt.url {
                return Err(ExecutorError::new(ExecutorErrorCode::PullRequestConflict));
            }
            return Ok(prior_receipt.clone());
        }
        persist_pull_receipt(&receipt_path, &request_hash, &receipt)?;
        return Ok(receipt);
    }
    if lookup_only {
        return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
    }
    if let Some(prior) = prior.as_ref() {
        return Err(ExecutorError::new(if prior.receipt.is_none() {
            ExecutorErrorCode::ForgeUnavailable
        } else {
            ExecutorErrorCode::PullRequestConflict
        }));
    }

    let post_url = forge_repository_url(forge, &request.repository, &["pulls"])?;
    let authorization = forge_authorization(forge.kind, &token)?;
    // Replays must reconcile this possible POST rather than create another PR.
    persist_pull_pending(&receipt_path, &request_hash)?;
    let response = client
        .post(post_url)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&serde_json::json!({
            "title": request.pull_request.title,
            "body": body,
            "head": request.branch,
            "base": request.base_branch,
        }))
        .send()
        .await
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::ForgeUnavailable))?;
    let status = response.status();
    let response_body = read_limited_response(response).await?;
    if !status.is_success() {
        if status == reqwest::StatusCode::CONFLICT
            || status == reqwest::StatusCode::UNPROCESSABLE_ENTITY
        {
            let existing = list_pull_requests(
                &client,
                &list_url,
                forge,
                &request.repository,
                &request.branch,
                &request.base_branch,
                &request.frozen_commit,
                &request.pull_request.title,
                &body,
                &marker,
                &token,
            )
            .await?;
            let Some(mut receipt) = existing else {
                return Err(ExecutorError::new(ExecutorErrorCode::PullRequestConflict));
            };
            receipt.run_id = request.run_id.clone();
            receipt.attempt_id = request.attempt_id.clone();
            receipt.repository = request.repository.clone();
            receipt.artifact_digest = request.artifact_digest.clone();
            receipt.already_present = true;
            persist_pull_receipt(&receipt_path, &request_hash, &receipt)?;
            return Ok(receipt);
        }
        if status.is_server_error() {
            return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
        }
        return Err(ExecutorError::new(ExecutorErrorCode::ForgeRejected));
    }
    let pull: ForgePull = serde_json::from_slice(&response_body)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::ForgeUnavailable))?;
    verify_pull(
        &pull,
        forge,
        &request.repository,
        &request.branch,
        &request.base_branch,
        &request.frozen_commit,
        &request.pull_request.title,
        &body,
        &marker,
    )?;
    let receipt = pull_receipt(request, &pull, false)?;
    persist_pull_receipt(&receipt_path, &request_hash, &receipt)?;
    Ok(receipt)
}

pub(crate) fn ensure_publisher(
    config: &ValidatedConfig,
    repository: &RepositoryConfig,
    run_id: &str,
) -> Result<PathBuf, ExecutorError> {
    let target = config.publisher_root.join(run_id);
    let staging = config.publisher_root.join(format!(".stage-{run_id}"));
    let git = GitContext::for_repository(&config.state_root, repository)?;
    let credential = repository
        .read_credential_source
        .as_deref()
        .map(|id| config.credential_source(id))
        .transpose()?;
    match fs::symlink_metadata(&target) {
        Ok(_) => {
            verify_existing_publisher(&git, repository, &target)?;
            Ok(target)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if fs::symlink_metadata(&staging).is_ok() {
                remove_private_path(&staging, &config.publisher_root)?;
            }
            let args = vec![
                "clone".into(),
                "--no-checkout".into(),
                "--no-tags".into(),
                "--no-local".into(),
                "--".into(),
                repository.clone_url.clone().into(),
                staging.as_os_str().to_owned(),
            ];
            if !git.run_silent(&config.publisher_root, &args, credential.as_ref())? {
                remove_private_path(&staging, &config.publisher_root)?;
                return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
            }
            fs::rename(&staging, &target)
                .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
            sync_directory(&config.publisher_root)?;
            Ok(target)
        }
        Err(_) => Err(ExecutorError::new(ExecutorErrorCode::IoFailure)),
    }
}

fn verify_existing_publisher(
    git: &GitContext,
    repository: &RepositoryConfig,
    target: &Path,
) -> Result<(), ExecutorError> {
    for path in [target.to_path_buf(), target.join(".git")] {
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
        }
    }
    let remote = git.checked(target, &strings(&["remote", "get-url", "origin"]), None)?;
    if remote != format!("{}\n", repository.clone_url).as_bytes() {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    Ok(())
}

fn remote_branch_head(
    git: &GitContext,
    publisher: &Path,
    branch: &str,
    credential: Option<&crate::ResolvedCredentialSource>,
) -> Result<Option<String>, ExecutorError> {
    let ref_name = format!("refs/heads/{branch}");
    let output = git.checked(
        publisher,
        &strings(&["ls-remote", "--heads", "origin", &ref_name]),
        credential,
    )?;
    let text = std::str::from_utf8(&output)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::GitOperationFailed))?;
    let mut found = None;
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let commit = fields
            .next()
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::GitOperationFailed))?;
        let ref_name_result = fields
            .next()
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::GitOperationFailed))?;
        if fields.next().is_some() || ref_name_result != ref_name {
            return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
        }
        if !matches!(commit.len(), 40 | 64)
            || !commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || found.replace(commit.to_owned()).is_some()
        {
            return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
        }
    }
    Ok(found)
}

pub(crate) fn forge_api_url(forge: &ForgeConfig, path: &[&str]) -> Result<Url, ExecutorError> {
    let mut url = Url::parse(&forge.api_base_url)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::InvalidConfiguration))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::InvalidConfiguration))?;
        segments.pop_if_empty();
        for segment in path {
            segments.push(segment);
        }
    }
    Ok(url)
}

pub(crate) fn forge_repository_url(
    forge: &ForgeConfig,
    repository: &crate::model::RepositoryId,
    suffix: &[&str],
) -> Result<Url, ExecutorError> {
    let mut path = vec!["repos", repository.owner.as_str(), repository.name.as_str()];
    path.extend_from_slice(suffix);
    forge_api_url(forge, &path)
}

pub(crate) fn forge_authorization(
    kind: ForgeKind,
    token: &str,
) -> Result<reqwest::header::HeaderValue, ExecutorError> {
    let scheme = match kind {
        ForgeKind::GitHub => "Bearer",
        ForgeKind::Gitea => "token",
    };
    reqwest::header::HeaderValue::from_str(&format!("{scheme} {token}"))
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::InvalidConfiguration))
}

pub(crate) fn forge_client(timeout: std::time::Duration) -> Result<reqwest::Client, ExecutorError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .user_agent(format!(
            "coding-agent-executor/{}/protocol-{}",
            crate::EXECUTOR_VERSION,
            crate::PROTOCOL_VERSION
        ))
        .build()
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::ForgeUnavailable))
}

#[allow(clippy::too_many_arguments)]
async fn list_pull_requests(
    client: &reqwest::Client,
    url: &Url,
    forge: &ForgeConfig,
    repository: &crate::model::RepositoryId,
    branch: &str,
    base_branch: &str,
    frozen_commit: &str,
    title: &str,
    body: &str,
    marker: &str,
    token: &str,
) -> Result<Option<PullRequestReceipt>, ExecutorError> {
    let authorization = forge_authorization(forge.kind, token)?;
    let mut matching = None;
    let mut page = 1;
    let mut scanned_bytes = 0_usize;
    let mut seen_pages = BTreeSet::new();
    let mut seen_pull_numbers = BTreeSet::new();
    loop {
        if page > MAX_PULL_REQUEST_PAGES {
            return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
        }
        let page_url = pull_list_page_url(url, forge.kind, repository, branch, base_branch, page);
        if !seen_pages.insert(page_url.as_str().to_owned()) {
            return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
        }
        let response = client
            .get(page_url.clone())
            .header(reqwest::header::AUTHORIZATION, authorization.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::ForgeUnavailable))?;
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            return Err(if status.is_server_error() || status.is_success() {
                ExecutorError::new(ExecutorErrorCode::ForgeUnavailable)
            } else {
                ExecutorError::new(ExecutorErrorCode::ForgeRejected)
            });
        }
        if response
            .headers()
            .contains_key(reqwest::header::CONTENT_RANGE)
        {
            return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
        }
        let next_link = next_link(response.headers())?;
        let bytes = read_limited_response(response).await?;
        scanned_bytes = scanned_bytes
            .checked_add(bytes.len())
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::ResponseTooLarge))?;
        if scanned_bytes > MAX_PULL_REQUEST_SCAN_BYTES {
            return Err(ExecutorError::new(ExecutorErrorCode::ResponseTooLarge));
        }
        let pulls: Vec<ForgePull> = serde_json::from_slice(&bytes)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::ForgeUnavailable))?;
        if pulls.len() > PULL_REQUEST_PAGE_SIZE {
            return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
        }
        for pull in pulls.iter() {
            if !seen_pull_numbers.insert(pull.number) {
                return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
            }
            let has_run_marker = pull
                .body
                .as_deref()
                .is_some_and(|pull_body| pull_body.contains(marker));
            if pull.head.reference != branch && !has_run_marker {
                continue;
            }
            verify_pull(
                pull,
                forge,
                repository,
                branch,
                base_branch,
                frozen_commit,
                title,
                body,
                marker,
            )?;
            if matching.is_some() {
                return Err(ExecutorError::new(ExecutorErrorCode::PullRequestConflict));
            }
            matching = Some(pull_receipt_fields(repository, pull, true)?);
        }

        let next_page = match next_link {
            Some(link) => {
                let expected =
                    pull_list_page_url(url, forge.kind, repository, branch, base_branch, page + 1);
                validate_next_link(&page_url, &link, &expected)?;
                Some(expected)
            }
            None if pulls.len() == PULL_REQUEST_PAGE_SIZE => Some(pull_list_page_url(
                url,
                forge.kind,
                repository,
                branch,
                base_branch,
                page + 1,
            )),
            None => None,
        };
        let Some(next_page_url) = next_page else {
            break;
        };
        if page == MAX_PULL_REQUEST_PAGES || seen_pages.contains(next_page_url.as_str()) {
            return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
        }
        page += 1;
    }
    Ok(matching)
}

fn pull_list_page_url(
    endpoint: &Url,
    kind: ForgeKind,
    repository: &crate::model::RepositoryId,
    branch: &str,
    base_branch: &str,
    page: usize,
) -> Url {
    let mut url = endpoint.clone();
    url.set_query(None);
    let mut query = url.query_pairs_mut();
    query.append_pair("state", "all");
    match kind {
        ForgeKind::GitHub => {
            query
                .append_pair("head", &format!("{}:{branch}", repository.owner))
                .append_pair("base", base_branch)
                .append_pair("per_page", &PULL_REQUEST_PAGE_SIZE.to_string());
        }
        ForgeKind::Gitea => {
            query
                .append_pair("base_branch", base_branch)
                .append_pair("limit", &PULL_REQUEST_PAGE_SIZE.to_string());
        }
    }
    query.append_pair("page", &page.to_string());
    drop(query);
    url
}

fn validate_next_link(current: &Url, link: &str, expected: &Url) -> Result<(), ExecutorError> {
    let target = current
        .join(link)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::ForgeUnavailable))?;
    if target.scheme() != expected.scheme()
        || target.host_str() != expected.host_str()
        || target.port_or_known_default() != expected.port_or_known_default()
        || target.path() != expected.path()
        || !target.username().is_empty()
        || target.password().is_some()
        || target.fragment().is_some()
        || query_pairs(&target) != query_pairs(expected)
    {
        return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
    }
    Ok(())
}

fn query_pairs(url: &Url) -> BTreeMap<(String, String), usize> {
    let mut pairs = BTreeMap::new();
    for (name, value) in url.query_pairs() {
        let count = pairs
            .entry((name.into_owned(), value.into_owned()))
            .or_insert(0_usize);
        *count += 1;
    }
    pairs
}

fn next_link(headers: &reqwest::header::HeaderMap) -> Result<Option<String>, ExecutorError> {
    let mut next = None;
    let mut count = 0;
    for header in headers.get_all(reqwest::header::LINK) {
        let header = header
            .to_str()
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::ForgeUnavailable))?;
        for entry in split_link_header(header)? {
            count += 1;
            if count > 32 {
                return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
            }
            let (href, is_next) = parse_link_entry(entry)?;
            if is_next && next.replace(href.to_owned()).is_some() {
                return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
            }
        }
    }
    Ok(next)
}

fn split_link_header(value: &str) -> Result<Vec<&str>, ExecutorError> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut in_angle = false;
    let mut in_quote = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_quote && character == '\\' {
            escaped = true;
            continue;
        }
        match character {
            '"' => in_quote = !in_quote,
            '<' if !in_quote && !in_angle => in_angle = true,
            '>' if !in_quote && in_angle => in_angle = false,
            '<' | '>' if !in_quote => {
                return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
            }
            ',' if !in_quote && !in_angle => {
                let entry = value[start..index].trim();
                if entry.is_empty() {
                    return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
                }
                entries.push(entry);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if in_angle || in_quote || escaped {
        return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
    }
    let last = value[start..].trim();
    if last.is_empty() {
        return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
    }
    entries.push(last);
    Ok(entries)
}

fn parse_link_entry(entry: &str) -> Result<(&str, bool), ExecutorError> {
    let invalid = || ExecutorError::new(ExecutorErrorCode::ForgeUnavailable);
    let entry = entry.trim();
    let rest = entry.strip_prefix('<').ok_or_else(invalid)?;
    let closing = rest.find('>').ok_or_else(invalid)?;
    let href = &rest[..closing];
    if href.is_empty()
        || href.chars().any(char::is_control)
        || href.chars().any(char::is_whitespace)
    {
        return Err(invalid());
    }
    let parameters = rest[closing + 1..].trim();
    let mut relations = None;
    if !parameters.is_empty() {
        let parameters = parameters.strip_prefix(';').ok_or_else(invalid)?.trim();
        for parameter in split_link_parameters(parameters)? {
            let (name, value) = parameter.split_once('=').ok_or_else(invalid)?;
            let name = name.trim();
            let value = value.trim();
            if name.is_empty() || value.is_empty() {
                return Err(invalid());
            }
            let value = if let Some(quoted) = value.strip_prefix('"') {
                let quoted = quoted.strip_suffix('"').ok_or_else(invalid)?;
                if quoted.contains('"') || quoted.contains('\\') {
                    return Err(invalid());
                }
                quoted
            } else {
                if value.chars().any(char::is_whitespace) || value.contains('"') {
                    return Err(invalid());
                }
                value
            };
            if name.eq_ignore_ascii_case("rel")
                && (value.split_ascii_whitespace().next().is_none()
                    || relations.replace(value).is_some())
            {
                return Err(invalid());
            }
        }
    }
    let is_next = relations.is_some_and(|relations| {
        relations
            .split_ascii_whitespace()
            .any(|relation| relation.eq_ignore_ascii_case("next"))
    });
    Ok((href, is_next))
}

fn split_link_parameters(value: &str) -> Result<Vec<&str>, ExecutorError> {
    let mut parameters = Vec::new();
    let mut start = 0;
    let mut in_quote = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_quote && character == '\\' {
            escaped = true;
            continue;
        }
        if character == '"' {
            in_quote = !in_quote;
        } else if character == ';' && !in_quote {
            let parameter = value[start..index].trim();
            if parameter.is_empty() {
                return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
            }
            parameters.push(parameter);
            start = index + character.len_utf8();
        }
    }
    if in_quote || escaped {
        return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
    }
    let last = value[start..].trim();
    if last.is_empty() {
        return Err(ExecutorError::new(ExecutorErrorCode::ForgeUnavailable));
    }
    parameters.push(last);
    Ok(parameters)
}

#[allow(clippy::too_many_arguments)]
fn verify_pull(
    pull: &ForgePull,
    forge: &ForgeConfig,
    repository: &crate::model::RepositoryId,
    branch: &str,
    base_branch: &str,
    frozen_commit: &str,
    title: &str,
    body: &str,
    marker: &str,
) -> Result<(), ExecutorError> {
    let pull_body = pull.body.as_deref().unwrap_or_default();
    let full_name = format!("{}/{}", repository.owner, repository.name);
    if pull.head.reference != branch
        || pull.base.reference != base_branch
        || pull.head.sha != frozen_commit
        || pull.head.repo.as_ref().map(|repo| repo.full_name.as_str()) != Some(full_name.as_str())
        || pull.base.repo.as_ref().map(|repo| repo.full_name.as_str()) != Some(full_name.as_str())
        || pull.title != title
        || pull_body != body
        || !pull_body.lines().any(|line| line == marker)
        || !(pull.state.eq_ignore_ascii_case("open") || pull.state.eq_ignore_ascii_case("closed"))
        || !url_matches_base(&pull.html_url, &forge.web_base_url)
    {
        return Err(ExecutorError::new(ExecutorErrorCode::PullRequestConflict));
    }
    Ok(())
}

fn pull_receipt(
    request: &CreatePullRequestRequest,
    pull: &ForgePull,
    already_present: bool,
) -> Result<PullRequestReceipt, ExecutorError> {
    Ok(PullRequestReceipt {
        run_id: request.run_id.clone(),
        attempt_id: request.attempt_id.clone(),
        repository: request.repository.clone(),
        number: pull.number,
        url: pull.html_url.clone(),
        head_commit: pull.head.sha.clone(),
        artifact_digest: request.artifact_digest.clone(),
        base_branch: request.base_branch.clone(),
        already_present,
    })
}

fn pull_receipt_fields(
    repository: &crate::model::RepositoryId,
    pull: &ForgePull,
    already_present: bool,
) -> Result<PullRequestReceipt, ExecutorError> {
    Ok(PullRequestReceipt {
        run_id: String::new(),
        attempt_id: String::new(),
        repository: repository.clone(),
        number: pull.number,
        url: pull.html_url.clone(),
        head_commit: pull.head.sha.clone(),
        artifact_digest: String::new(),
        base_branch: pull.base.reference.clone(),
        already_present,
    })
}

fn pull_request_hash(request: &CreatePullRequestRequest) -> Result<String, ExecutorError> {
    let bytes = serde_json::to_vec(&(
        &request.run_id,
        &request.attempt_id,
        &request.repository,
        &request.branch,
        &request.base_branch,
        &request.base_commit,
        &request.artifact_digest,
        &request.frozen_commit,
        &request.pull_request.title,
        &request.pull_request.body,
    ))
    .map_err(|_| ExecutorError::new(ExecutorErrorCode::InvalidRequest))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn persist_pull_receipt(
    path: &Path,
    request_hash: &str,
    receipt: &PullRequestReceipt,
) -> Result<(), ExecutorError> {
    write_json_atomic(
        path,
        &PullRequestRecord {
            request_hash: request_hash.to_owned(),
            receipt: Some(receipt.clone()),
        },
    )
}

fn persist_pull_pending(path: &Path, request_hash: &str) -> Result<(), ExecutorError> {
    write_json_atomic(
        path,
        &PullRequestRecord {
            request_hash: request_hash.to_owned(),
            receipt: None,
        },
    )
}

fn run_marker(run_id: &str) -> String {
    format!("{RUN_MARKER_PREFIX}{run_id} -->")
}

fn contains_unapproved_close_directive(body: &str) -> bool {
    const KEYWORDS: [&[u8]; 9] = [
        b"close",
        b"closes",
        b"closed",
        b"fix",
        b"fixes",
        b"fixed",
        b"resolve",
        b"resolves",
        b"resolved",
    ];
    let body = body.to_ascii_lowercase();
    let bytes = body.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if index > 0 && is_directive_word_byte(bytes[index - 1]) {
            index += 1;
            continue;
        }
        let mut keyword_end = None;
        for keyword in KEYWORDS {
            let end = index + keyword.len();
            if bytes.get(index..end) != Some(keyword)
                || bytes
                    .get(end)
                    .is_some_and(|byte| is_directive_word_byte(*byte))
            {
                continue;
            }
            keyword_end = Some(end);
            break;
        }
        if let Some(end) = keyword_end {
            let reference_start = close_reference_start(bytes, end);
            if contains_potential_issue_reference(bytes, reference_start) {
                return true;
            }
            index = end;
        } else {
            index += 1;
        }
    }
    false
}

fn close_reference_start(bytes: &[u8], keyword_end: usize) -> usize {
    let mut start = keyword_end;
    while bytes.get(start).is_some_and(u8::is_ascii_whitespace) {
        start += 1;
    }
    if bytes.get(start) == Some(&b':') {
        start += 1;
        while bytes.get(start).is_some_and(u8::is_ascii_whitespace) {
            start += 1;
        }
    }
    start
}

fn contains_potential_issue_reference(bytes: &[u8], start: usize) -> bool {
    if start >= bytes.len() {
        return false;
    }
    let mut start = start;
    while bytes
        .get(start)
        .is_some_and(|byte| matches!(byte, b'*' | b'_' | b'~'))
    {
        start += 1;
    }
    if start >= bytes.len() {
        return false;
    }
    if bytes[start..].starts_with(b"http://")
        || bytes[start..].starts_with(b"https://")
        || bytes[start..].starts_with(b"www.")
        || matches!(bytes[start], b'[' | b'<' | b'`')
        || bytes[start..].starts_with(b"![")
        || bytes[start..].starts_with(b"\\[")
    {
        return true;
    }

    let token_start = if bytes[start] == b'(' {
        let mut inner = start + 1;
        while bytes.get(inner).is_some_and(u8::is_ascii_whitespace) {
            inner += 1;
        }
        if inner < bytes.len()
            && (bytes[inner..].starts_with(b"http://")
                || bytes[inner..].starts_with(b"https://")
                || matches!(bytes[inner], b'[' | b'<' | b'`'))
        {
            return true;
        }
        inner
    } else {
        start
    };
    let token_end = bytes[token_start..]
        .iter()
        .position(|byte| byte.is_ascii_whitespace() || matches!(byte, b',' | b';'))
        .map_or(bytes.len(), |offset| token_start + offset);
    let Some(hash) = bytes[token_start..token_end]
        .iter()
        .position(|byte| *byte == b'#')
    else {
        return false;
    };
    let number_start = token_start + hash + 1;
    let number_end = number_start
        + bytes[number_start..token_end]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
    number_end > number_start
        && (number_end == token_end
            || bytes
                .get(number_end)
                .is_some_and(|byte| !is_directive_word_byte(*byte)))
}

fn is_directive_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn url_matches_base(candidate: &str, base: &str) -> bool {
    let Ok(candidate) = Url::parse(candidate) else {
        return false;
    };
    let Ok(base) = Url::parse(base) else {
        return false;
    };
    candidate.scheme() == base.scheme()
        && candidate.host_str() == base.host_str()
        && candidate.port_or_known_default() == base.port_or_known_default()
        && candidate.username().is_empty()
        && candidate.password().is_none()
        && candidate.fragment().is_none()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ForgeResponseReadError {
    ResponseTooLarge,
    Timeout,
    Unavailable,
}

pub(crate) async fn read_limited_response_reason(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, ForgeResponseReadError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(ForgeResponseReadError::ResponseTooLarge);
    }
    let mut result = Vec::new();
    loop {
        let chunk = response.chunk().await.map_err(|error| {
            if error.is_timeout() {
                ForgeResponseReadError::Timeout
            } else {
                ForgeResponseReadError::Unavailable
            }
        })?;
        let Some(chunk) = chunk else { break };
        if result.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ForgeResponseReadError::ResponseTooLarge);
        }
        result.extend_from_slice(&chunk);
    }
    Ok(result)
}

pub(crate) async fn read_limited_response_with_limit(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, ExecutorError> {
    read_limited_response_reason(response, max_bytes)
        .await
        .map_err(|error| {
            ExecutorError::new(match error {
                ForgeResponseReadError::ResponseTooLarge => ExecutorErrorCode::ResponseTooLarge,
                ForgeResponseReadError::Timeout | ForgeResponseReadError::Unavailable => {
                    ExecutorErrorCode::ForgeUnavailable
                }
            })
        })
}

async fn read_limited_response(response: reqwest::Response) -> Result<Vec<u8>, ExecutorError> {
    read_limited_response_with_limit(response, MAX_FORGE_RESPONSE_BYTES).await
}

fn is_lower_hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sync_directory(path: &Path) -> Result<(), ExecutorError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))
}

fn remove_private_path(path: &Path, root: &Path) -> Result<(), ExecutorError> {
    if !path.starts_with(root) || path == root {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(ExecutorError::new(ExecutorErrorCode::IoFailure)),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))
    } else {
        fs::remove_file(path).map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))
    }
}

#[derive(Debug, Deserialize)]
struct ForgePull {
    number: u64,
    title: String,
    body: Option<String>,
    html_url: String,
    state: String,
    head: ForgeReference,
    base: ForgeReference,
}

#[derive(Debug, Deserialize)]
struct ForgeReference {
    #[serde(rename = "ref")]
    reference: String,
    sha: String,
    repo: Option<ForgeRepository>,
}

#[derive(Debug, Deserialize)]
struct ForgeRepository {
    full_name: String,
}
