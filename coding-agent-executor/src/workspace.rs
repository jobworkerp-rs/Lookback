use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fs2::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::git_context::{GitContext, strings};
use crate::model::{
    BaseResolutionReceipt, ExecutorError, ExecutorErrorCode, PrepareRequest, PreparedWorkspace,
    RepositoryId, ResolveBaseRequest,
};
use crate::{ValidatedConfig, validate_full_commit, validate_run_id};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MAX_BASE_TREE_LISTING_BYTES: usize = 16 * 1024 * 1024;
const MAX_BASE_TREE_ENTRIES: usize = 65_536;
const MAX_BASE_RESOLUTION_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct BaseResolutionRecord {
    request: ResolveBaseRequest,
    receipt: BaseResolutionReceipt,
}

/// Resolves and records a remote base without cloning or otherwise preparing a workspace.
pub(crate) fn resolve_base(
    config: &ValidatedConfig,
    request: &ResolveBaseRequest,
) -> Result<BaseResolutionReceipt, ExecutorError> {
    validate_run_id(&request.run_id)?;
    validate_run_id(&request.attempt_id)?;
    validate_run_id(&request.operation_id)?;
    let repository = config.repository(&request.repository)?;
    if !request.base_branch.is_empty() {
        config.validate_base_branch(repository, &request.base_branch)?;
    }

    let _lock = acquire_run_lock(config, &request.run_id)?;
    let resolution_dir = ensure_run_state_dir(config, &request.run_id)?.join("base-resolutions");
    ensure_private_dir(&resolution_dir)?;
    let receipt_path = resolution_dir.join(format!("{}.json", request.operation_id));
    if let Some(record) = read_json::<BaseResolutionRecord>(&receipt_path)? {
        if record.request != *request {
            return Err(ExecutorError::new(ExecutorErrorCode::IdempotencyConflict));
        }
        if !base_resolution_receipt_matches(config, request, &record.receipt) {
            return Err(ExecutorError::new(ExecutorErrorCode::IoFailure));
        }
        return Ok(record.receipt);
    }

    let credential = repository
        .read_credential_source
        .as_deref()
        .map(|id| config.credential_source(id))
        .transpose()?;
    let git = GitContext::for_repository(&config.state_root, repository)?;
    let base_branch = if request.base_branch.is_empty() {
        let output = git.run_limited(
            &config.workspace_root,
            &strings(&[
                "ls-remote",
                "--symref",
                "--exit-code",
                "--",
                &repository.clone_url,
                "HEAD",
            ]),
            credential.as_ref(),
            MAX_BASE_RESOLUTION_OUTPUT_BYTES,
        )?;
        if !output.success {
            return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
        }
        let output = std::str::from_utf8(&output.stdout)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::GitOperationFailed))?;
        let lines: Vec<_> = output.lines().collect();
        let [symbolic, head] = lines.as_slice() else {
            return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
        };
        let branch = symbolic
            .strip_prefix("ref: refs/heads/")
            .and_then(|line| line.strip_suffix("\tHEAD"))
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::GitOperationFailed))?;
        parse_base_resolution_output(format!("{head}\n").as_bytes(), "HEAD")?;
        config.validate_base_branch(repository, branch)?;
        branch.to_owned()
    } else {
        request.base_branch.clone()
    };
    let remote_ref = format!("refs/heads/{base_branch}");
    if !git.run_silent(
        &config.workspace_root,
        &strings(&["check-ref-format", &remote_ref]),
        None,
    )? {
        return Err(ExecutorError::new(ExecutorErrorCode::InvalidBranch));
    }
    let args = strings(&[
        "ls-remote",
        "--exit-code",
        "--heads",
        "--",
        &repository.clone_url,
        &remote_ref,
    ]);
    let output = git.run_limited(
        &config.workspace_root,
        &args,
        credential.as_ref(),
        MAX_BASE_RESOLUTION_OUTPUT_BYTES,
    )?;
    if !output.success {
        return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
    }
    let base_commit = parse_base_resolution_output(&output.stdout, &remote_ref)?;
    let receipt = BaseResolutionReceipt {
        run_id: request.run_id.clone(),
        attempt_id: request.attempt_id.clone(),
        operation_id: request.operation_id.clone(),
        repository: request.repository.clone(),
        base_branch,
        base_commit,
        trusted_config_digest: config.trusted_config_digest.clone(),
        protocol_version: crate::PROTOCOL_VERSION,
        executor_version: crate::EXECUTOR_VERSION.to_owned(),
    };
    write_json_atomic(
        &receipt_path,
        &BaseResolutionRecord {
            request: request.clone(),
            receipt: receipt.clone(),
        },
    )?;
    Ok(receipt)
}

fn parse_base_resolution_output(
    output: &[u8],
    expected_ref: &str,
) -> Result<String, ExecutorError> {
    let invalid = || ExecutorError::new(ExecutorErrorCode::GitOperationFailed);
    let line = output.strip_suffix(b"\n").ok_or_else(invalid)?;
    if line.is_empty() || line.contains(&b'\n') || line.contains(&b'\r') {
        return Err(invalid());
    }
    let separator = line
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(invalid)?;
    let (commit, remote_ref) = line.split_at(separator);
    if remote_ref.get(1..) != Some(expected_ref.as_bytes()) {
        return Err(invalid());
    }
    let commit = std::str::from_utf8(commit).map_err(|_| invalid())?;
    validate_full_commit(commit).map_err(|_| invalid())?;
    Ok(commit.to_owned())
}

fn base_resolution_receipt_matches(
    config: &ValidatedConfig,
    request: &ResolveBaseRequest,
    receipt: &BaseResolutionReceipt,
) -> bool {
    receipt.run_id == request.run_id
        && receipt.attempt_id == request.attempt_id
        && receipt.operation_id == request.operation_id
        && receipt.repository == request.repository
        && (request.base_branch.is_empty() || receipt.base_branch == request.base_branch)
        && config.repository(&receipt.repository).is_ok_and(|repo| {
            config
                .validate_base_branch(repo, &receipt.base_branch)
                .is_ok()
        })
        && validate_full_commit(&receipt.base_commit).is_ok()
        && receipt.trusted_config_digest == config.trusted_config_digest
        && receipt.protocol_version == crate::PROTOCOL_VERSION
        && receipt.executor_version == crate::EXECUTOR_VERSION
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedRecord {
    pub(crate) request: PrepareRequest,
    status: PrepareStatus,
    pub(crate) result: PreparedWorkspace,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PrepareStatus {
    Preparing,
    Promoting,
    Ready,
}

pub(crate) struct WorkspaceManager<'a> {
    config: &'a ValidatedConfig,
    git: GitContext,
}

impl<'a> WorkspaceManager<'a> {
    pub(crate) fn new(config: &'a ValidatedConfig) -> Result<Self, ExecutorError> {
        Ok(Self {
            config,
            git: GitContext::new(&config.state_root)?,
        })
    }

    pub(crate) fn prepare(
        &self,
        request: &PrepareRequest,
    ) -> Result<PreparedWorkspace, ExecutorError> {
        validate_run_id(&request.run_id)?;
        validate_run_id(&request.attempt_id)?;
        validate_full_commit(&request.base_commit)?;
        let _lock = acquire_run_lock(self.config, &request.run_id)?;
        let attempt_dir =
            ensure_attempt_state_dir(self.config, &request.run_id, &request.attempt_id)?;
        let record_path = attempt_dir.join("prepared.json");
        let workspace = self.workspace_path(&request.run_id, &request.attempt_id);
        let staging = self.staging_path(&request.run_id, &request.attempt_id);
        let workspace_parent = workspace
            .parent()
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
        ensure_workspace_parent(workspace_parent)?;

        if let Some(mut record) = read_json::<PreparedRecord>(&record_path)? {
            if record.request != *request {
                return Err(ExecutorError::new(ExecutorErrorCode::IdempotencyConflict));
            }
            match record.status {
                PrepareStatus::Ready => {
                    self.validate_owned_workspace(&workspace, &record.result)?;
                    return Ok(record.result);
                }
                PrepareStatus::Preparing => {
                    remove_owned_path(&staging, &self.config.workspace_root)?;
                }
                PrepareStatus::Promoting => {
                    let stage_exists = staging.exists();
                    let workspace_exists = workspace.exists();
                    match (stage_exists, workspace_exists) {
                        (true, false) => {
                            fs::rename(&staging, &workspace)
                                .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
                            sync_directory(workspace_parent)?;
                        }
                        (false, true) => {}
                        _ => return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace)),
                    }
                    record.status = PrepareStatus::Ready;
                    self.validate_owned_workspace(&workspace, &record.result)?;
                    write_json_atomic(&record_path, &record)?;
                    return Ok(record.result);
                }
            }
        } else if workspace.exists() || staging.exists() {
            return Err(ExecutorError::new(
                ExecutorErrorCode::WorkspaceAlreadyExists,
            ));
        }

        if workspace.exists() || staging.exists() {
            return Err(ExecutorError::new(
                ExecutorErrorCode::WorkspaceAlreadyExists,
            ));
        }

        let repository = self.config.repository(&request.repository)?;
        let git = GitContext::for_repository(&self.config.state_root, repository)?;
        let credential = repository
            .read_credential_source
            .as_deref()
            .map(|id| self.config.credential_source(id))
            .transpose()?;
        let result = PreparedWorkspace {
            run_id: request.run_id.clone(),
            attempt_id: request.attempt_id.clone(),
            repository: request.repository.clone(),
            base_branch: request.base_branch.clone(),
            base_commit: request.base_commit.clone(),
            branch: request.expected_branch(),
            workspace_path: workspace
                .to_str()
                .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::InvalidConfiguration))?
                .to_owned(),
        };
        let mut record = PreparedRecord {
            request: request.clone(),
            status: PrepareStatus::Preparing,
            result: result.clone(),
        };
        write_json_atomic(&record_path, &record)?;

        let args = vec![
            "clone".into(),
            "--no-checkout".into(),
            "--no-tags".into(),
            "--no-local".into(),
            "--".into(),
            repository.clone_url.clone().into(),
            staging.as_os_str().to_owned(),
        ];
        if !git.run_silent(&self.config.workspace_root, &args, credential.as_ref())? {
            return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
        }

        fetch_base_branch(&git, &staging, &request.base_branch, credential.as_ref())?;
        verify_commit_type(&self.git, &staging, &request.base_commit)?;
        let remote_ref = format!("refs/remotes/origin/{}", request.base_branch);
        let ancestor = self.git.run(
            &staging,
            &strings(&[
                "merge-base",
                "--is-ancestor",
                &request.base_commit,
                &remote_ref,
            ]),
            None,
        )?;
        if !ancestor.success {
            return Err(ExecutorError::new(
                ExecutorErrorCode::CommitNotBasedOnApprovedBase,
            ));
        }
        verify_supported_base_tree(&self.git, &staging, &request.base_commit)?;
        let branch = request.expected_branch();
        let checked_out = self.git.run_silent(
            &staging,
            &strings(&[
                "checkout",
                "--no-guess",
                "-b",
                &branch,
                &request.base_commit,
            ]),
            None,
        )?;
        if !checked_out {
            return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
        }
        sync_tree(&staging)?;

        record.status = PrepareStatus::Promoting;
        write_json_atomic(&record_path, &record)?;
        fs::rename(&staging, &workspace)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
        sync_directory(workspace_parent)?;
        record.status = PrepareStatus::Ready;
        write_json_atomic(&record_path, &record)?;
        self.validate_owned_workspace(&workspace, &result)?;
        Ok(result)
    }

    fn workspace_path(&self, run_id: &str, attempt_id: &str) -> PathBuf {
        self.config.workspace_root.join(run_id).join(attempt_id)
    }

    fn staging_path(&self, run_id: &str, attempt_id: &str) -> PathBuf {
        self.config
            .workspace_root
            .join(format!(".stage-{run_id}-{attempt_id}"))
    }

    fn validate_owned_workspace(
        &self,
        path: &Path,
        result: &PreparedWorkspace,
    ) -> Result<(), ExecutorError> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::WorkspaceNotPrepared))?;
        let canonical = fs::canonicalize(path)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?;
        let expected = self.workspace_path(&result.run_id, &result.attempt_id);
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || canonical != expected
            || !canonical.starts_with(&self.config.workspace_root)
            || result.workspace_path != expected.to_string_lossy()
        {
            return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
        }
        Ok(())
    }
}

fn verify_supported_base_tree(
    git: &GitContext,
    repository: &Path,
    commit: &str,
) -> Result<(), ExecutorError> {
    let object_format = git.checked(
        repository,
        &strings(&["rev-parse", "--show-object-format"]),
        None,
    )?;
    let oid_len = match object_format.as_slice() {
        b"sha1\n" => 40,
        b"sha256\n" => 64,
        _ => return Err(ExecutorError::new(ExecutorErrorCode::InvalidCommit)),
    };
    if commit.len() != oid_len {
        return Err(ExecutorError::new(ExecutorErrorCode::InvalidCommit));
    }

    let args = strings(&["ls-tree", "-r", "-t", "-z", "--full-tree", commit]);
    let listing = match git.checked_limited(repository, &args, None, MAX_BASE_TREE_LISTING_BYTES) {
        Ok(listing) => listing,
        Err(error) if error.code == ExecutorErrorCode::ResponseTooLarge => {
            return Err(ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge));
        }
        Err(error) => return Err(error),
    };
    validate_base_tree_listing(&listing, oid_len)
}

fn validate_base_tree_listing(listing: &[u8], oid_len: usize) -> Result<(), ExecutorError> {
    if listing.is_empty() {
        return Ok(());
    }
    if !listing.ends_with(&[0]) {
        return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
    }

    let mut count = 0;
    for record in listing[..listing.len() - 1].split(|byte| *byte == 0) {
        if record.is_empty() {
            return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
        }
        count += 1;
        if count > MAX_BASE_TREE_ENTRIES {
            return Err(ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge));
        }
        let Some(separator) = record.iter().position(|byte| *byte == b'\t') else {
            return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
        };
        let mut header = record[..separator].split(|byte| *byte == b' ');
        let (Some(mode), Some(kind), Some(oid)) = (header.next(), header.next(), header.next())
        else {
            return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
        };
        let supported_mode = (mode == b"100644" || mode == b"100755") && kind == b"blob"
            || mode == b"040000" && kind == b"tree";
        if header.next().is_some()
            || oid.len() != oid_len
            || !oid.iter().all(u8::is_ascii_hexdigit)
            || !supported_mode
        {
            return Err(ExecutorError::new(ExecutorErrorCode::UnsupportedBaseTree));
        }
    }
    Ok(())
}

pub(crate) fn acquire_run_lock(
    config: &ValidatedConfig,
    run_id: &str,
) -> Result<File, ExecutorError> {
    validate_run_id(run_id)?;
    let lock_dir = config.state_root.join("locks");
    ensure_private_dir(&lock_dir)?;
    let path = lock_dir.join(format!("{run_id}.lock"));
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    file.lock_exclusive()
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    Ok(file)
}

pub(crate) fn ensure_run_state_dir(
    config: &ValidatedConfig,
    run_id: &str,
) -> Result<PathBuf, ExecutorError> {
    validate_run_id(run_id)?;
    let runs = config.state_root.join("runs");
    ensure_private_dir(&runs)?;
    let path = runs.join(run_id);
    ensure_private_dir(&path)?;
    Ok(path)
}

pub(crate) fn ensure_attempt_state_dir(
    config: &ValidatedConfig,
    run_id: &str,
    attempt_id: &str,
) -> Result<PathBuf, ExecutorError> {
    validate_run_id(run_id)?;
    validate_run_id(attempt_id)?;
    let run_dir = ensure_run_state_dir(config, run_id)?;
    let attempts = run_dir.join("attempts");
    ensure_private_dir(&attempts)?;
    let path = attempts.join(attempt_id);
    ensure_private_dir(&path)?;
    Ok(path)
}

pub(crate) fn prepared_record(
    config: &ValidatedConfig,
    run_id: &str,
    attempt_id: &str,
    repository: &RepositoryId,
    base_branch: &str,
    base_commit: &str,
) -> Result<(PreparedRecord, PathBuf), ExecutorError> {
    let attempt_dir = ensure_attempt_state_dir(config, run_id, attempt_id)?;
    let record = read_json::<PreparedRecord>(&attempt_dir.join("prepared.json"))?
        .filter(|record| record.status == PrepareStatus::Ready)
        .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::WorkspaceNotPrepared))?;
    if record.request.attempt_id != attempt_id
        || record.request.repository != *repository
        || record.request.base_branch != base_branch
        || record.request.base_commit != base_commit
    {
        return Err(ExecutorError::new(ExecutorErrorCode::IdempotencyConflict));
    }
    let path = config.workspace_root.join(run_id).join(attempt_id);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::WorkspaceNotPrepared))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    let canonical = fs::canonicalize(&path)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?;
    if canonical != path
        || !canonical.starts_with(&config.workspace_root)
        || record.result.workspace_path != path.to_string_lossy()
    {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    Ok((record, canonical))
}

pub(crate) fn prepared_metadata_record(
    config: &ValidatedConfig,
    run_id: &str,
    attempt_id: &str,
    repository: &RepositoryId,
    base_branch: &str,
    base_commit: &str,
) -> Result<PreparedRecord, ExecutorError> {
    let attempt_dir = ensure_attempt_state_dir(config, run_id, attempt_id)?;
    let record = read_json::<PreparedRecord>(&attempt_dir.join("prepared.json"))?
        .filter(|record| record.status == PrepareStatus::Ready)
        .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::WorkspaceNotPrepared))?;
    let expected_path = config.workspace_root.join(run_id).join(attempt_id);
    if record.request.run_id != run_id
        || record.request.attempt_id != attempt_id
        || record.request.repository != *repository
        || record.request.base_branch != base_branch
        || record.request.base_commit != base_commit
        || record.result.workspace_path != expected_path.to_string_lossy()
        || record.result.branch != record.request.expected_branch()
    {
        return Err(ExecutorError::new(ExecutorErrorCode::IdempotencyConflict));
    }
    Ok(record)
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, ExecutorError> {
    match open_read_nofollow(path) {
        Ok(file) => {
            if !file
                .metadata()
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= 1024 * 1024)
            {
                return Err(ExecutorError::new(ExecutorErrorCode::IoFailure));
            }
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(ExecutorError::new(ExecutorErrorCode::IoFailure)),
    }
}

pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), ExecutorError> {
    let parent = path
        .parent()
        .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    let bytes =
        serde_json::to_vec(value).map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".receipt-{}-{sequence}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options
        .open(&temp)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    fs::rename(&temp, path).map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    sync_directory(parent)
}

fn remove_owned_path(path: &Path, root: &Path) -> Result<(), ExecutorError> {
    if !path.starts_with(root) || path == root {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))
    } else {
        fs::remove_file(path).map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))
    }
}

fn sync_tree(path: &Path) -> Result<(), ExecutorError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    if metadata.file_type().is_symlink() {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    if metadata.is_dir() {
        for entry in
            fs::read_dir(path).map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?
        {
            let entry = entry.map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
            sync_tree(&entry.path())?;
        }
        sync_directory(path)
    } else if metadata.is_file() {
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))
    } else {
        Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))
    }
}

fn sync_directory(path: &Path) -> Result<(), ExecutorError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))
}

fn ensure_workspace_parent(path: &Path) -> Result<(), ExecutorError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
                        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
                }
                sync_directory(path)?;
                sync_directory(
                    path.parent()
                        .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::IoFailure))?,
                )
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(path)
                    .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
                if metadata.is_dir() && !metadata.file_type().is_symlink() {
                    Ok(())
                } else {
                    Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))
                }
            }
            Err(_) => Err(ExecutorError::new(ExecutorErrorCode::IoFailure)),
        },
        Err(_) => Err(ExecutorError::new(ExecutorErrorCode::IoFailure)),
    }
}

pub(crate) fn ensure_private_dir(path: &Path) -> Result<(), ExecutorError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
        }
        Err(_) => return Err(ExecutorError::new(ExecutorErrorCode::IoFailure)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    }
    sync_directory(path)?;
    sync_directory(
        path.parent()
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::IoFailure))?,
    )?;
    Ok(())
}

fn open_read_nofollow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    options.open(path)
}

pub(crate) fn string_args(parts: &[&str]) -> Vec<std::ffi::OsString> {
    strings(parts)
}

pub(crate) fn fetch_base_branch(
    git: &GitContext,
    repository: &Path,
    base_branch: &str,
    credential: Option<&crate::ResolvedCredentialSource>,
) -> Result<(), ExecutorError> {
    let src = format!("+refs/heads/{base_branch}:refs/remotes/origin/{base_branch}");
    let args = vec![
        "fetch".into(),
        "--no-tags".into(),
        "origin".into(),
        src.into(),
    ];
    if git.run_silent(repository, &args, credential)? {
        Ok(())
    } else {
        Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed))
    }
}

pub(crate) fn verify_commit_type(
    git: &GitContext,
    repository: &Path,
    commit: &str,
) -> Result<(), ExecutorError> {
    let output = git.checked(repository, &string_args(&["cat-file", "-t", commit]), None)?;
    if output == b"commit\n" {
        Ok(())
    } else {
        Err(ExecutorError::new(ExecutorErrorCode::InvalidCommit))
    }
}
