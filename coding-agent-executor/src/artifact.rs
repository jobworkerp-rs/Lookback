use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::git_context::{GitContext, strings};
use crate::model::{
    CaptureGuarantee, CaptureReceipt, CaptureRequest, ExecutorError, ExecutorErrorCode,
    PushRequest, ResolveRequest, ResolvedArtifact, SealRequest, SealedArtifactReceipt,
};
use crate::publication::ensure_publisher;
use crate::workspace::{
    acquire_run_lock, ensure_attempt_state_dir, fetch_base_branch, prepared_metadata_record,
    prepared_record, read_json, verify_commit_type, write_json_atomic,
};
use crate::{ValidatedConfig, validate_full_commit, validate_run_id};

const MANIFEST_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SNAPSHOT_ENTRIES: usize = 65_536;
const MAX_SNAPSHOT_NAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_SNAPSHOT_PATH_BYTES: usize = 16 * 1024 * 1024;
const MAX_SNAPSHOT_PATH_LENGTH: usize = 4096;
const MAX_GIT_STORAGE_SYNC_ENTRIES: usize = 1_000_000;
const MAX_GIT_STORAGE_SYNC_DEPTH: usize = 128;
const EXCLUDED_ROOT_PATHS: [&str; 2] = [".git", ".coding-agent"];
static ARTIFACT_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SnapshotManifest {
    version: u32,
    excluded_root_paths: Vec<String>,
    entries: Vec<SnapshotEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SnapshotEntry {
    path: String,
    kind: SnapshotEntryKind,
    byte_count: u64,
    sha256: Option<String>,
    executable: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SnapshotEntryKind {
    Directory,
    File,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CaptureRecord {
    request: CaptureRequest,
    receipt: CaptureReceipt,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SealRecord {
    receipt: SealedArtifactReceipt,
}

struct SnapshotAccumulator {
    entries: Vec<SnapshotEntry>,
    estimated_manifest_bytes: usize,
    file_count: u64,
    byte_count: u64,
    max_bytes: u64,
    budget: SnapshotBudget,
}

#[derive(Clone, Copy)]
struct SnapshotBudgetLimits {
    max_entries: usize,
    max_name_bytes: usize,
    max_path_bytes: usize,
}

struct SnapshotBudget {
    limits: SnapshotBudgetLimits,
    entry_count: usize,
    name_bytes: usize,
    path_bytes: usize,
}

impl SnapshotBudget {
    fn new() -> Self {
        Self::with_limits(SnapshotBudgetLimits {
            max_entries: MAX_SNAPSHOT_ENTRIES,
            max_name_bytes: MAX_SNAPSHOT_NAME_BYTES,
            max_path_bytes: MAX_SNAPSHOT_PATH_BYTES,
        })
    }

    fn with_limits(limits: SnapshotBudgetLimits) -> Self {
        Self {
            limits,
            entry_count: 0,
            name_bytes: 0,
            path_bytes: 0,
        }
    }

    fn record_entry(&mut self, name_len: usize, path_len: usize) -> Result<(), ExecutorError> {
        let entry_count = self
            .entry_count
            .checked_add(1)
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge))?;
        let name_bytes = self
            .name_bytes
            .checked_add(name_len)
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge))?;
        let path_bytes = self
            .path_bytes
            .checked_add(path_len)
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge))?;
        if entry_count > self.limits.max_entries
            || name_bytes > self.limits.max_name_bytes
            || path_bytes > self.limits.max_path_bytes
            || path_len > MAX_SNAPSHOT_PATH_LENGTH
        {
            return Err(ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge));
        }
        self.entry_count = entry_count;
        self.name_bytes = name_bytes;
        self.path_bytes = path_bytes;
        Ok(())
    }
}

#[derive(Serialize)]
struct ArtifactIdentity<'a> {
    run_id: &'a str,
    attempt_id: &'a str,
    repository: &'a crate::model::RepositoryId,
    base_branch: &'a str,
    base_commit: &'a str,
    approved_snapshot_digest: &'a str,
    scope_digest: &'a str,
    intent_approval_digest: &'a str,
    manifest_digest: &'a str,
    finalized_bytes_digest: &'a str,
    tree: &'a str,
    commit: &'a str,
    protocol_version: u32,
    executor_version: &'a str,
    capture_guarantee: CaptureGuarantee,
}

pub(crate) fn capture(
    config: &ValidatedConfig,
    request: &CaptureRequest,
) -> Result<CaptureReceipt, ExecutorError> {
    validate_capture_request(request)?;
    let _lock = acquire_run_lock(config, &request.run_id)?;
    let attempt_dir = ensure_attempt_state_dir(config, &request.run_id, &request.attempt_id)?;
    let capture_path = attempt_dir.join("capture.json");
    if let Some(prior) = read_json::<CaptureRecord>(&capture_path)? {
        if prior.request != *request {
            return Err(ExecutorError::new(ExecutorErrorCode::IdempotencyConflict));
        }
        verify_capture(config, &attempt_dir, &prior)?;
        return Ok(prior.receipt);
    }
    let (prepared, workspace) = prepared_record(
        config,
        &request.run_id,
        &request.attempt_id,
        &request.repository,
        &request.base_branch,
        &request.base_commit,
    )?;
    if prepared.request.run_id != request.run_id
        || prepared.request.attempt_id != request.attempt_id
        || prepared.request.repository != request.repository
        || prepared.request.base_branch != request.base_branch
        || prepared.request.base_commit != request.base_commit
    {
        return Err(ExecutorError::new(ExecutorErrorCode::IdempotencyConflict));
    }

    let data_path = attempt_dir.join("capture-data");
    let manifest_path = attempt_dir.join("manifest.json");
    match fs::symlink_metadata(&data_path) {
        Ok(_) => return Err(artifact_integrity_failure()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(io_failure()),
    }
    remove_private_path(&manifest_path, &attempt_dir)?;
    let staging = attempt_dir.join(".capture-stage");
    remove_capture_staging(&staging, &attempt_dir)?;
    fs::create_dir(&staging).map_err(|_| io_failure())?;
    set_private_directory_mode(&staging, 0o700)?;

    let snapshot = match copy_workspace(config, &workspace, &staging) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            remove_capture_staging(&staging, &attempt_dir)?;
            return Err(error);
        }
    };
    let mut manifest = SnapshotManifest {
        version: MANIFEST_VERSION,
        excluded_root_paths: EXCLUDED_ROOT_PATHS
            .iter()
            .map(|path| (*path).to_owned())
            .collect(),
        entries: snapshot.entries,
    };
    manifest
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    let manifest_bytes = serde_json::to_vec(&manifest).map_err(|_| io_failure())?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        remove_capture_staging(&staging, &attempt_dir)?;
        return Err(ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge));
    }
    let manifest_digest = sha256_hex(&manifest_bytes);
    let verified = match verify_manifest_data_root(config, &staging, &manifest) {
        Ok(verified) => verified,
        Err(error) => {
            remove_capture_staging(&staging, &attempt_dir)?;
            return Err(error);
        }
    };
    if verified.file_count != snapshot.file_count || verified.byte_count != snapshot.byte_count {
        remove_capture_staging(&staging, &attempt_dir)?;
        return Err(artifact_integrity_failure());
    }
    let finalized_bytes_digest = verified.finalized_bytes_digest;

    if let Err(error) = sync_tree(&staging) {
        remove_capture_staging(&staging, &attempt_dir)?;
        return Err(error);
    }
    fs::rename(&staging, &data_path).map_err(|_| io_failure())?;
    write_bytes_atomic(&manifest_path, &manifest_bytes)?;
    sync_directory(&attempt_dir)?;
    let receipt = CaptureReceipt {
        run_id: request.run_id.clone(),
        attempt_id: request.attempt_id.clone(),
        repository: request.repository.clone(),
        base_branch: request.base_branch.clone(),
        base_commit: request.base_commit.clone(),
        approved_snapshot_digest: request.approved_snapshot_digest.clone(),
        scope_digest: request.scope_digest.clone(),
        intent_approval_digest: request.intent_approval_digest.clone(),
        manifest_digest,
        finalized_bytes_digest,
        file_count: snapshot.file_count,
        byte_count: snapshot.byte_count,
        capture_guarantee: CaptureGuarantee::HostIsolatedAfterSeal,
    };
    write_json_atomic(
        &capture_path,
        &CaptureRecord {
            request: request.clone(),
            receipt: receipt.clone(),
        },
    )?;
    Ok(receipt)
}

pub(crate) fn seal(
    config: &ValidatedConfig,
    request: &SealRequest,
) -> Result<SealedArtifactReceipt, ExecutorError> {
    seal_with_git_sync(config, request, sync_git_storage)
}

fn seal_with_git_sync<F>(
    config: &ValidatedConfig,
    request: &SealRequest,
    sync_git: F,
) -> Result<SealedArtifactReceipt, ExecutorError>
where
    F: FnOnce(&Path) -> Result<(), ExecutorError>,
{
    validate_run_id(&request.run_id)?;
    validate_run_id(&request.attempt_id)?;
    validate_digest(&request.manifest_digest)?;
    let _lock = acquire_run_lock(config, &request.run_id)?;
    let attempt_dir = ensure_attempt_state_dir(config, &request.run_id, &request.attempt_id)?;
    let capture_record = load_capture(config, &attempt_dir)?;
    if capture_record.receipt.manifest_digest != request.manifest_digest {
        return Err(ExecutorError::new(ExecutorErrorCode::ArtifactMismatch));
    }
    let seal_path = attempt_dir.join("seal.json");
    if let Some(prior) = read_json::<SealRecord>(&seal_path)? {
        verify_seal(config, &attempt_dir, &capture_record, &prior.receipt)?;
        return Ok(prior.receipt);
    }

    let manifest = read_manifest(&attempt_dir, &capture_record.receipt)?;
    let repository = config.repository(&capture_record.receipt.repository)?;
    let publisher = ensure_publisher(config, repository, &request.run_id)?;
    let git = GitContext::for_repository(&config.state_root, repository)?;
    let credential = repository
        .read_credential_source
        .as_deref()
        .map(|id| config.credential_source(id))
        .transpose()?;
    fetch_base_branch(
        &git,
        &publisher,
        &capture_record.receipt.base_branch,
        credential.as_ref(),
    )?;
    verify_commit_type(&git, &publisher, &capture_record.receipt.base_commit)?;
    verify_base_approval(
        &git,
        &publisher,
        &capture_record.receipt.base_branch,
        &capture_record.receipt.base_commit,
    )?;
    let (tree, commit) = create_native_commit(
        &git,
        &publisher,
        &attempt_dir.join("capture-data"),
        &manifest,
        &capture_record.receipt,
    )?;
    sync_git(&publisher)?;
    let mut receipt = SealedArtifactReceipt {
        run_id: capture_record.receipt.run_id.clone(),
        attempt_id: capture_record.receipt.attempt_id.clone(),
        repository: capture_record.receipt.repository.clone(),
        base_branch: capture_record.receipt.base_branch.clone(),
        base_commit: capture_record.receipt.base_commit.clone(),
        approved_snapshot_digest: capture_record.receipt.approved_snapshot_digest.clone(),
        scope_digest: capture_record.receipt.scope_digest.clone(),
        intent_approval_digest: capture_record.receipt.intent_approval_digest.clone(),
        manifest_digest: capture_record.receipt.manifest_digest.clone(),
        finalized_bytes_digest: capture_record.receipt.finalized_bytes_digest.clone(),
        tree,
        commit,
        artifact_digest: String::new(),
        protocol_version: crate::model::PROTOCOL_VERSION,
        executor_version: crate::model::EXECUTOR_VERSION.to_owned(),
        capture_guarantee: CaptureGuarantee::HostIsolatedAfterSeal,
    };
    receipt.artifact_digest = artifact_digest(&receipt)?;
    verify_seal(config, &attempt_dir, &capture_record, &receipt)?;
    write_json_atomic(
        &seal_path,
        &SealRecord {
            receipt: receipt.clone(),
        },
    )?;
    Ok(receipt)
}

pub(crate) fn resolve(
    config: &ValidatedConfig,
    request: &ResolveRequest,
) -> Result<ResolvedArtifact, ExecutorError> {
    validate_run_id(&request.run_id)?;
    validate_run_id(&request.attempt_id)?;
    validate_digest(&request.artifact_digest)?;
    let _lock = acquire_run_lock(config, &request.run_id)?;
    let attempt_dir = ensure_attempt_state_dir(config, &request.run_id, &request.attempt_id)?;
    let capture_record = load_capture(config, &attempt_dir)?;
    let seal_record = load_seal(config, &attempt_dir, &capture_record)?;
    if seal_record.artifact_digest != request.artifact_digest {
        return Err(ExecutorError::new(ExecutorErrorCode::ArtifactMismatch));
    }
    verify_seal(config, &attempt_dir, &capture_record, &seal_record)?;
    let prepared = prepared_metadata_record(
        config,
        &request.run_id,
        &request.attempt_id,
        &seal_record.repository,
        &seal_record.base_branch,
        &seal_record.base_commit,
    )?;
    Ok(ResolvedArtifact {
        run_id: seal_record.run_id,
        attempt_id: seal_record.attempt_id,
        repository: seal_record.repository,
        branch: prepared.result.branch,
        base_branch: seal_record.base_branch,
        base_commit: seal_record.base_commit,
        approved_snapshot_digest: seal_record.approved_snapshot_digest,
        scope_digest: seal_record.scope_digest,
        intent_approval_digest: seal_record.intent_approval_digest,
        manifest_digest: seal_record.manifest_digest,
        finalized_bytes_digest: seal_record.finalized_bytes_digest,
        tree: seal_record.tree,
        commit: seal_record.commit,
        artifact_digest: seal_record.artifact_digest,
        protocol_version: seal_record.protocol_version,
        executor_version: seal_record.executor_version,
        capture_guarantee: seal_record.capture_guarantee,
    })
}

pub(crate) fn require_push_artifact(
    config: &ValidatedConfig,
    request: &PushRequest,
) -> Result<SealedArtifactReceipt, ExecutorError> {
    validate_digest(&request.artifact_digest)?;
    validate_full_commit(&request.frozen_commit)?;
    let attempt_dir = ensure_attempt_state_dir(config, &request.run_id, &request.attempt_id)?;
    let capture_record = load_capture(config, &attempt_dir)?;
    let receipt = load_seal(config, &attempt_dir, &capture_record)?;
    if receipt.repository != request.repository
        || receipt.base_branch != request.base_branch
        || receipt.base_commit != request.base_commit
        || receipt.artifact_digest != request.artifact_digest
        || receipt.commit != request.frozen_commit
    {
        return Err(ExecutorError::new(ExecutorErrorCode::ArtifactMismatch));
    }
    verify_seal(config, &attempt_dir, &capture_record, &receipt)?;
    Ok(receipt)
}

fn validate_capture_request(request: &CaptureRequest) -> Result<(), ExecutorError> {
    validate_run_id(&request.run_id)?;
    validate_run_id(&request.attempt_id)?;
    validate_full_commit(&request.base_commit)?;
    for digest in [
        &request.approved_snapshot_digest,
        &request.scope_digest,
        &request.intent_approval_digest,
    ] {
        validate_digest(digest)?;
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<(), ExecutorError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ExecutorError::new(ExecutorErrorCode::InvalidRequest));
    }
    Ok(())
}

fn load_capture(
    config: &ValidatedConfig,
    attempt_dir: &Path,
) -> Result<CaptureRecord, ExecutorError> {
    let record = read_json::<CaptureRecord>(&attempt_dir.join("capture.json"))?
        .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::ArtifactNotSealed))?;
    verify_capture(config, attempt_dir, &record)?;
    Ok(record)
}

fn verify_capture(
    config: &ValidatedConfig,
    attempt_dir: &Path,
    record: &CaptureRecord,
) -> Result<(), ExecutorError> {
    validate_capture_request(&record.request)?;
    if record.receipt.run_id != record.request.run_id
        || record.receipt.attempt_id != record.request.attempt_id
        || record.receipt.repository != record.request.repository
        || record.receipt.base_branch != record.request.base_branch
        || record.receipt.base_commit != record.request.base_commit
        || record.receipt.approved_snapshot_digest != record.request.approved_snapshot_digest
        || record.receipt.scope_digest != record.request.scope_digest
        || record.receipt.intent_approval_digest != record.request.intent_approval_digest
        || record.receipt.capture_guarantee != CaptureGuarantee::HostIsolatedAfterSeal
    {
        return Err(ExecutorError::new(
            ExecutorErrorCode::ArtifactIntegrityFailure,
        ));
    }
    let manifest = read_manifest(attempt_dir, &record.receipt)?;
    let stats = verify_manifest_data_root(config, &attempt_dir.join("capture-data"), &manifest)?;
    if stats.file_count != record.receipt.file_count
        || stats.byte_count != record.receipt.byte_count
        || stats.finalized_bytes_digest != record.receipt.finalized_bytes_digest
    {
        return Err(ExecutorError::new(
            ExecutorErrorCode::ArtifactIntegrityFailure,
        ));
    }
    Ok(())
}

fn read_manifest(
    attempt_dir: &Path,
    receipt: &CaptureReceipt,
) -> Result<SnapshotManifest, ExecutorError> {
    let bytes = read_limited_file(&attempt_dir.join("manifest.json"), MAX_MANIFEST_BYTES)?;
    if sha256_hex(&bytes) != receipt.manifest_digest {
        return Err(ExecutorError::new(
            ExecutorErrorCode::ArtifactIntegrityFailure,
        ));
    }
    let manifest: SnapshotManifest = serde_json::from_slice(&bytes)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::ArtifactIntegrityFailure))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &SnapshotManifest) -> Result<(), ExecutorError> {
    if manifest.version != MANIFEST_VERSION
        || manifest.excluded_root_paths
            != EXCLUDED_ROOT_PATHS
                .iter()
                .map(|path| (*path).to_owned())
                .collect::<Vec<_>>()
        || manifest.entries.len() > MAX_SNAPSHOT_ENTRIES
    {
        return Err(ExecutorError::new(
            ExecutorErrorCode::ArtifactIntegrityFailure,
        ));
    }
    let mut previous = None;
    let mut seen = BTreeSet::new();
    for entry in &manifest.entries {
        if !safe_manifest_path(&entry.path)
            || entry.path.len() > 4096
            || entry.path.matches('/').count() > 128
            || previous.is_some_and(|value: &str| value >= entry.path.as_str())
            || !seen.insert(entry.path.as_str())
        {
            return Err(ExecutorError::new(
                ExecutorErrorCode::ArtifactIntegrityFailure,
            ));
        }
        previous = Some(entry.path.as_str());
        match entry.kind {
            SnapshotEntryKind::Directory
                if entry.byte_count != 0 || entry.sha256.is_some() || entry.executable =>
            {
                return Err(ExecutorError::new(
                    ExecutorErrorCode::ArtifactIntegrityFailure,
                ));
            }
            SnapshotEntryKind::File
                if entry
                    .sha256
                    .as_deref()
                    .is_none_or(|digest| validate_digest(digest).is_err()) =>
            {
                return Err(ExecutorError::new(
                    ExecutorErrorCode::ArtifactIntegrityFailure,
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn safe_manifest_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == ".." || part == ".git")
        && !EXCLUDED_ROOT_PATHS
            .iter()
            .any(|excluded| path == *excluded || path.starts_with(&format!("{excluded}/")))
}

#[derive(PartialEq)]
struct VerifiedData {
    file_count: u64,
    byte_count: u64,
    finalized_bytes_digest: String,
}

fn verify_manifest_data_root(
    config: &ValidatedConfig,
    root: &Path,
    manifest: &SnapshotManifest,
) -> Result<VerifiedData, ExecutorError> {
    let mut expected_paths = BTreeSet::new();
    let mut finalized_hasher = Sha256::new();
    let mut file_count = 0_u64;
    let mut byte_count = 0_u64;
    let mut remaining = config.max_import_bytes;
    for entry in &manifest.entries {
        expected_paths.insert(entry.path.clone());
        let path = root.join(&entry.path);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::ArtifactIntegrityFailure))?;
        match entry.kind {
            SnapshotEntryKind::Directory => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(ExecutorError::new(
                        ExecutorErrorCode::ArtifactIntegrityFailure,
                    ));
                }
            }
            SnapshotEntryKind::File => {
                if !metadata.is_file()
                    || metadata.file_type().is_symlink()
                    || metadata.len() != entry.byte_count
                    || entry.byte_count > remaining
                {
                    return Err(ExecutorError::new(
                        ExecutorErrorCode::ArtifactIntegrityFailure,
                    ));
                }
                remaining -= entry.byte_count;
                let mut file = open_read_nofollow(&path)
                    .map_err(|_| ExecutorError::new(ExecutorErrorCode::ArtifactIntegrityFailure))?;
                let mut file_hasher = Sha256::new();
                let mut data_bytes = 0_u64;
                let mut buffer = [0_u8; 64 * 1024];
                update_finalized_prefix(
                    &mut finalized_hasher,
                    &entry.path,
                    entry.executable,
                    entry.byte_count,
                );
                loop {
                    let read = file
                        .read(&mut buffer)
                        .map_err(|_| artifact_integrity_failure())?;
                    if read == 0 {
                        break;
                    }
                    data_bytes = data_bytes
                        .checked_add(read as u64)
                        .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge))?;
                    if data_bytes > entry.byte_count {
                        return Err(artifact_integrity_failure());
                    }
                    file_hasher.update(&buffer[..read]);
                    finalized_hasher.update(&buffer[..read]);
                }
                if data_bytes != entry.byte_count
                    || encode_hex(&file_hasher.finalize())
                        != entry.sha256.as_deref().unwrap_or_default()
                {
                    return Err(ExecutorError::new(
                        ExecutorErrorCode::ArtifactIntegrityFailure,
                    ));
                }
                file_count += 1;
                byte_count = byte_count
                    .checked_add(entry.byte_count)
                    .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge))?;
            }
        }
    }
    let actual_paths = enumerate_artifact_paths(root)?;
    if actual_paths != expected_paths {
        return Err(ExecutorError::new(
            ExecutorErrorCode::ArtifactIntegrityFailure,
        ));
    }
    Ok(VerifiedData {
        file_count,
        byte_count,
        finalized_bytes_digest: encode_hex(&finalized_hasher.finalize()),
    })
}

fn load_seal(
    config: &ValidatedConfig,
    attempt_dir: &Path,
    capture: &CaptureRecord,
) -> Result<SealedArtifactReceipt, ExecutorError> {
    let record = read_json::<SealRecord>(&attempt_dir.join("seal.json"))?
        .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::ArtifactNotSealed))?;
    verify_seal(config, attempt_dir, capture, &record.receipt)?;
    Ok(record.receipt)
}

fn verify_seal(
    config: &ValidatedConfig,
    attempt_dir: &Path,
    capture: &CaptureRecord,
    receipt: &SealedArtifactReceipt,
) -> Result<(), ExecutorError> {
    let source = &capture.receipt;
    if receipt.run_id != source.run_id
        || receipt.attempt_id != source.attempt_id
        || receipt.repository != source.repository
        || receipt.base_branch != source.base_branch
        || receipt.base_commit != source.base_commit
        || receipt.approved_snapshot_digest != source.approved_snapshot_digest
        || receipt.scope_digest != source.scope_digest
        || receipt.intent_approval_digest != source.intent_approval_digest
        || receipt.manifest_digest != source.manifest_digest
        || receipt.finalized_bytes_digest != source.finalized_bytes_digest
        || receipt.protocol_version != crate::model::PROTOCOL_VERSION
        || receipt.executor_version != crate::model::EXECUTOR_VERSION
        || receipt.capture_guarantee != CaptureGuarantee::HostIsolatedAfterSeal
        || artifact_digest(receipt)? != receipt.artifact_digest
    {
        return Err(ExecutorError::new(
            ExecutorErrorCode::ArtifactIntegrityFailure,
        ));
    }
    verify_capture(config, attempt_dir, capture)?;
    let repository = config.repository(&receipt.repository)?;
    let publisher = config.publisher_root.join(&receipt.run_id);
    let metadata = fs::symlink_metadata(&publisher)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::ArtifactIntegrityFailure))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ExecutorError::new(
            ExecutorErrorCode::ArtifactIntegrityFailure,
        ));
    }
    let git = GitContext::new(&config.state_root)?;
    verify_commit_type(&git, &publisher, &receipt.commit)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::ArtifactIntegrityFailure))?;
    let tree = commit_tree_id(&git, &publisher, &receipt.commit)?;
    let parent = commit_parent_id(&git, &publisher, &receipt.commit)?;
    if tree != receipt.tree || parent != receipt.base_commit {
        return Err(ExecutorError::new(
            ExecutorErrorCode::ArtifactIntegrityFailure,
        ));
    }
    let _ = repository;
    Ok(())
}

fn artifact_digest(receipt: &SealedArtifactReceipt) -> Result<String, ExecutorError> {
    let identity = ArtifactIdentity {
        run_id: &receipt.run_id,
        attempt_id: &receipt.attempt_id,
        repository: &receipt.repository,
        base_branch: &receipt.base_branch,
        base_commit: &receipt.base_commit,
        approved_snapshot_digest: &receipt.approved_snapshot_digest,
        scope_digest: &receipt.scope_digest,
        intent_approval_digest: &receipt.intent_approval_digest,
        manifest_digest: &receipt.manifest_digest,
        finalized_bytes_digest: &receipt.finalized_bytes_digest,
        tree: &receipt.tree,
        commit: &receipt.commit,
        protocol_version: receipt.protocol_version,
        executor_version: &receipt.executor_version,
        capture_guarantee: receipt.capture_guarantee,
    };
    let encoded = serde_json::to_vec(&identity).map_err(|_| io_failure())?;
    Ok(sha256_hex(&encoded))
}

fn create_native_commit(
    git: &GitContext,
    publisher: &Path,
    data_root: &Path,
    manifest: &SnapshotManifest,
    capture: &CaptureReceipt,
) -> Result<(String, String), ExecutorError> {
    let entry_by_path: BTreeMap<&str, &SnapshotEntry> = manifest
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut children = BTreeMap::<String, Vec<(String, bool, String)>>::new();
    let mut directories = Vec::new();
    for entry in &manifest.entries {
        let (parent, leaf) = entry.path.rsplit_once('/').unwrap_or(("", &entry.path));
        match entry.kind {
            SnapshotEntryKind::Directory => directories.push(entry.path.clone()),
            SnapshotEntryKind::File => children.entry(parent.to_owned()).or_default().push((
                leaf.to_owned(),
                false,
                entry.path.clone(),
            )),
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path_depth(path)));
    for directory in directories {
        let (parent, leaf) = directory.rsplit_once('/').unwrap_or(("", &directory));
        if children
            .get(&directory)
            .is_some_and(|items| !items.is_empty())
        {
            children
                .entry(parent.to_owned())
                .or_default()
                .push((leaf.to_owned(), true, directory));
        }
    }

    let mut trees = BTreeMap::<String, String>::new();
    let mut tree_paths: Vec<String> = children.keys().cloned().collect();
    tree_paths.sort_by_key(|path| std::cmp::Reverse(path_depth(path)));
    if !tree_paths.iter().any(String::is_empty) {
        tree_paths.push(String::new());
    }
    for directory in tree_paths {
        let mut input = Vec::new();
        if let Some(items) = children.get(&directory) {
            for (name, is_dir, path) in items {
                if *is_dir {
                    let Some(tree) = trees.get(path) else {
                        continue;
                    };
                    input.extend_from_slice(b"040000 tree ");
                    input.extend_from_slice(tree.as_bytes());
                    input.push(b'\t');
                    input.extend_from_slice(name.as_bytes());
                    input.push(0);
                } else {
                    let entry = entry_by_path
                        .get(path.as_str())
                        .copied()
                        .ok_or_else(artifact_integrity_failure)?;
                    let file = open_read_nofollow(&data_root.join(path))
                        .map_err(|_| artifact_integrity_failure())?;
                    let object = git.run_with_reader(
                        publisher,
                        &strings(&["hash-object", "-w", "--stdin"]),
                        None,
                        file,
                        128,
                    )?;
                    if !object.success {
                        return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
                    }
                    let oid = parse_oid(&object.stdout)?;
                    let mode = if entry.executable {
                        "100755 blob "
                    } else {
                        "100644 blob "
                    };
                    input.extend_from_slice(mode.as_bytes());
                    input.extend_from_slice(oid.as_bytes());
                    input.push(b'\t');
                    input.extend_from_slice(name.as_bytes());
                    input.push(0);
                }
            }
        }
        let output = git.run_with_input(publisher, &strings(&["mktree", "-z"]), None, &input)?;
        if !output.success {
            return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
        }
        trees.insert(directory, parse_oid(&output.stdout)?);
    }
    let tree = trees
        .get("")
        .cloned()
        .ok_or_else(artifact_integrity_failure)?;
    let message = format!(
        "Agent workspace result for run {} attempt {}",
        capture.run_id, capture.attempt_id
    );
    let args = vec![
        "-c".into(),
        "user.name=Coding Agent Executor".into(),
        "-c".into(),
        "user.email=coding-agent-executor@invalid".into(),
        "commit-tree".into(),
        tree.clone().into(),
        "-p".into(),
        capture.base_commit.clone().into(),
        "-m".into(),
        message.into(),
    ];
    let output = git.run(publisher, &args, None)?;
    if !output.success {
        return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
    }
    let commit = parse_oid(&output.stdout)?;
    Ok((tree, commit))
}

fn verify_base_approval(
    git: &GitContext,
    publisher: &Path,
    base_branch: &str,
    base_commit: &str,
) -> Result<(), ExecutorError> {
    let remote_ref = format!("refs/remotes/origin/{base_branch}");
    let ancestor = git.run(
        publisher,
        &strings(&["merge-base", "--is-ancestor", base_commit, &remote_ref]),
        None,
    )?;
    if !ancestor.success {
        return Err(ExecutorError::new(
            ExecutorErrorCode::CommitNotBasedOnApprovedBase,
        ));
    }
    Ok(())
}

fn commit_tree_id(
    git: &GitContext,
    repository: &Path,
    commit: &str,
) -> Result<String, ExecutorError> {
    let revision = format!("{commit}^{{tree}}");
    parse_oid(&git.checked(
        repository,
        &strings(&["rev-parse", "--verify", "--end-of-options", &revision]),
        None,
    )?)
}

fn commit_parent_id(
    git: &GitContext,
    repository: &Path,
    commit: &str,
) -> Result<String, ExecutorError> {
    let revision = format!("{commit}^1");
    parse_oid(&git.checked(
        repository,
        &strings(&["rev-parse", "--verify", "--end-of-options", &revision]),
        None,
    )?)
}

fn parse_oid(bytes: &[u8]) -> Result<String, ExecutorError> {
    let oid = std::str::from_utf8(bytes)
        .map_err(|_| artifact_integrity_failure())?
        .trim();
    validate_full_commit(oid).map_err(|_| artifact_integrity_failure())?;
    Ok(oid.to_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    encode_hex(&Sha256::digest(bytes))
}

fn path_depth(path: &str) -> usize {
    if path.is_empty() {
        0
    } else {
        path.matches('/').count() + 1
    }
}

fn update_finalized_prefix(hasher: &mut Sha256, path: &str, executable: bool, byte_count: u64) {
    hasher.update((path.len() as u64).to_be_bytes());
    hasher.update(path.as_bytes());
    hasher.update(if executable { b"100755" } else { b"100644" });
    hasher.update(byte_count.to_be_bytes());
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn artifact_integrity_failure() -> ExecutorError {
    ExecutorError::new(ExecutorErrorCode::ArtifactIntegrityFailure)
}

fn io_failure() -> ExecutorError {
    ExecutorError::new(ExecutorErrorCode::IoFailure)
}

fn read_limited_file(path: &Path, limit: u64) -> Result<Vec<u8>, ExecutorError> {
    let file = open_read_nofollow(path).map_err(|_| artifact_integrity_failure())?;
    if !file
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= limit)
    {
        return Err(artifact_integrity_failure());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| artifact_integrity_failure())?;
    if bytes.len() as u64 > limit {
        return Err(artifact_integrity_failure());
    }
    Ok(bytes)
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), ExecutorError> {
    let parent = path.parent().ok_or_else(io_failure)?;
    let sequence = ARTIFACT_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".manifest-{}-{sequence}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&temporary).map_err(|_| io_failure())?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| io_failure())?;
    fs::rename(&temporary, path).map_err(|_| io_failure())?;
    sync_directory(parent)
}

fn sync_tree(path: &Path) -> Result<(), ExecutorError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| io_failure())?;
    if metadata.file_type().is_symlink() {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path).map_err(|_| io_failure())? {
            sync_tree(&entry.map_err(|_| io_failure())?.path())?;
        }
        sync_directory(path)?;
    } else if metadata.is_file() {
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|_| io_failure())?;
    } else {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), ExecutorError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| io_failure())
}

#[cfg(target_os = "linux")]
fn sync_git_storage(publisher: &Path) -> Result<(), ExecutorError> {
    use std::os::fd::AsRawFd;

    let publisher_dir = open_directory(publisher)?;
    let git_dir_name = std::ffi::CString::new(".git")
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?;
    let git_dir = openat_directory(publisher_dir.as_raw_fd(), &git_dir_name)?;
    let mut entry_count = 0;
    sync_git_directory(&git_dir, &mut entry_count, 0)?;
    publisher_dir.sync_all().map_err(|_| io_failure())
}

#[cfg(not(target_os = "linux"))]
fn sync_git_storage(_publisher: &Path) -> Result<(), ExecutorError> {
    Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))
}

#[cfg(target_os = "linux")]
fn sync_git_directory(
    directory: &File,
    entry_count: &mut usize,
    depth: usize,
) -> Result<(), ExecutorError> {
    use std::os::fd::AsRawFd;

    if depth > MAX_GIT_STORAGE_SYNC_DEPTH {
        return Err(ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge));
    }
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(io_failure());
    }
    let handle = unsafe { libc::fdopendir(duplicate) };
    if handle.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(io_failure());
    }
    let handle = DirectoryStream(handle);
    loop {
        unsafe { *libc::__errno_location() = 0 };
        let entry = unsafe { libc::readdir(handle.0) };
        if entry.is_null() {
            if unsafe { *libc::__errno_location() } != 0 {
                return Err(io_failure());
            }
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        *entry_count = entry_count
            .checked_add(1)
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge))?;
        if *entry_count > MAX_GIT_STORAGE_SYNC_ENTRIES {
            return Err(ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge));
        }
        let name = std::ffi::CString::new(name)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?;
        let before = stat_at(directory.as_raw_fd(), &name)?;
        match before.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                let child = openat_directory(directory.as_raw_fd(), &name)?;
                let opened = file_stat(&child)?;
                ensure_same_file(&before, &opened)?;
                sync_git_directory(&child, entry_count, depth + 1)?;
                let after = stat_at(directory.as_raw_fd(), &name)?;
                ensure_same_file(&opened, &after)?;
            }
            libc::S_IFREG if before.st_nlink == 1 => {
                let file = openat_file(directory.as_raw_fd(), &name)?;
                let opened = file_stat(&file)?;
                ensure_same_file(&before, &opened)?;
                file.sync_all().map_err(|_| io_failure())?;
                let after = stat_at(directory.as_raw_fd(), &name)?;
                ensure_same_file(&opened, &after)?;
            }
            _ => return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace)),
        }
    }
    directory.sync_all().map_err(|_| io_failure())
}

#[cfg(target_os = "linux")]
struct DirectoryStream(*mut libc::DIR);

#[cfg(target_os = "linux")]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

fn set_private_directory_mode(path: &Path, mode: u32) -> Result<(), ExecutorError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|_| io_failure())?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn remove_private_path(path: &Path, root: &Path) -> Result<(), ExecutorError> {
    if !path.starts_with(root) || path == root {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(|_| io_failure())
    } else {
        fs::remove_file(path).map_err(|_| io_failure())
    }
}

fn remove_capture_staging(path: &Path, attempt_dir: &Path) -> Result<(), ExecutorError> {
    if path != attempt_dir.join(".capture-stage") {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    remove_capture_staging_entry(path)
}

fn remove_capture_staging_entry(path: &Path) -> Result<(), ExecutorError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(io_failure()),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return fs::remove_file(path).map_err(|_| io_failure());
    }

    set_private_directory_mode(path, 0o700)?;
    for entry in fs::read_dir(path).map_err(|_| io_failure())? {
        let entry = entry.map_err(|_| io_failure())?;
        remove_capture_staging_entry(&entry.path())?;
    }
    fs::remove_dir(path).map_err(|_| io_failure())
}

fn enumerate_artifact_paths(root: &Path) -> Result<BTreeSet<String>, ExecutorError> {
    let mut paths = BTreeSet::new();
    let metadata = fs::symlink_metadata(root).map_err(|_| artifact_integrity_failure())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(artifact_integrity_failure());
    }
    let mut budget = SnapshotBudget::new();
    enumerate_artifact_dir(root, "", &mut paths, &mut budget)?;
    Ok(paths)
}

fn enumerate_artifact_dir(
    directory: &Path,
    prefix: &str,
    paths: &mut BTreeSet<String>,
    budget: &mut SnapshotBudget,
) -> Result<(), ExecutorError> {
    for entry in fs::read_dir(directory).map_err(|_| artifact_integrity_failure())? {
        let entry = entry.map_err(|_| artifact_integrity_failure())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| artifact_integrity_failure())?;
        let path_len = if prefix.is_empty() {
            name.len()
        } else {
            prefix
                .len()
                .checked_add(1)
                .and_then(|length| length.checked_add(name.len()))
                .ok_or_else(artifact_integrity_failure)?
        };
        budget
            .record_entry(name.len(), path_len)
            .map_err(|_| artifact_integrity_failure())?;
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|_| artifact_integrity_failure())?;
        if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
            return Err(artifact_integrity_failure());
        }
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if !paths.insert(relative.clone()) {
            return Err(artifact_integrity_failure());
        }
        if metadata.is_dir() {
            enumerate_artifact_dir(&entry.path(), &relative, paths, budget)?;
        }
    }
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

#[cfg(target_os = "linux")]
fn copy_workspace(
    config: &ValidatedConfig,
    _workspace: &Path,
    destination: &Path,
) -> Result<SnapshotAccumulator, ExecutorError> {
    use std::os::fd::AsRawFd;

    let root = open_directory(&config.workspace_root)?;
    let run_id = std::ffi::CString::new(
        _workspace
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?,
    )
    .map_err(|_| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?;
    let attempt_id = std::ffi::CString::new(
        _workspace
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?,
    )
    .map_err(|_| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?;
    let run = openat_directory(root.as_raw_fd(), &run_id)?;
    let source = openat_directory(run.as_raw_fd(), &attempt_id)?;
    let mut accumulator = SnapshotAccumulator {
        entries: Vec::new(),
        estimated_manifest_bytes: 128,
        file_count: 0,
        byte_count: 0,
        max_bytes: config.max_import_bytes,
        budget: SnapshotBudget::new(),
    };
    copy_directory_contents(&source, destination, "", true, &mut accumulator)?;
    set_private_directory_mode(destination, 0o500)?;
    Ok(accumulator)
}

#[cfg(not(target_os = "linux"))]
fn copy_workspace(
    _config: &ValidatedConfig,
    _workspace: &Path,
    _destination: &Path,
) -> Result<SnapshotAccumulator, ExecutorError> {
    Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))
}

#[cfg(target_os = "linux")]
fn copy_directory_contents(
    source: &File,
    destination: &Path,
    prefix: &str,
    root: bool,
    accumulator: &mut SnapshotAccumulator,
) -> Result<(), ExecutorError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let before_directory = file_stat(source)?;
    let mut names = read_directory_names(source, prefix, root, &mut accumulator.budget)?;
    names.sort();
    for name_bytes in names {
        let name = std::str::from_utf8(&name_bytes)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?;
        if name.is_empty()
            || name_bytes.len() > 255
            || name.contains('/')
            || name == "."
            || name == ".."
            || prefix.matches('/').count() >= 128
        {
            return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
        }
        if root && EXCLUDED_ROOT_PATHS.contains(&name) {
            continue;
        }
        if name == ".git" {
            return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
        }
        let name_c = std::ffi::CString::new(name_bytes.clone())
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?;
        let before = stat_at(source.as_raw_fd(), &name_c)?;
        let relative = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        match before.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                let child = openat_directory(source.as_raw_fd(), &name_c)?;
                let opened = file_stat(&child)?;
                ensure_same_file(&before, &opened)?;
                let target = destination.join(name);
                fs::create_dir(&target).map_err(|_| io_failure())?;
                copy_directory_contents(&child, &target, &relative, false, accumulator)?;
                let after = stat_at(source.as_raw_fd(), &name_c)?;
                ensure_same_file(&before, &after)?;
                fs::set_permissions(&target, fs::Permissions::from_mode(0o500))
                    .map_err(|_| io_failure())?;
                push_manifest_entry(
                    accumulator,
                    SnapshotEntry {
                        path: relative,
                        kind: SnapshotEntryKind::Directory,
                        byte_count: 0,
                        sha256: None,
                        executable: false,
                    },
                )?;
            }
            libc::S_IFREG => {
                if before.st_nlink != 1 || before.st_size < 0 {
                    return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
                }
                if before.st_size as u64
                    > accumulator.max_bytes.saturating_sub(accumulator.byte_count)
                {
                    return Err(ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge));
                }
                let file = openat_file(source.as_raw_fd(), &name_c)?;
                let opened = file_stat(&file)?;
                ensure_same_file(&before, &opened)?;
                let target = destination.join(name);
                let mut output_options = OpenOptions::new();
                output_options.write(true).create_new(true);
                #[cfg(unix)]
                output_options.mode(0o600);
                let mut output = output_options.open(&target).map_err(|_| io_failure())?;
                let mut file_hasher = Sha256::new();
                let mut file_bytes = 0_u64;
                let mut buffer = [0_u8; 64 * 1024];
                let mut input = file;
                loop {
                    let read = input.read(&mut buffer).map_err(|_| io_failure())?;
                    if read == 0 {
                        break;
                    }
                    file_bytes = file_bytes
                        .checked_add(read as u64)
                        .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge))?;
                    if file_bytes > before.st_size as u64
                        || file_bytes > accumulator.max_bytes.saturating_sub(accumulator.byte_count)
                    {
                        return Err(ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge));
                    }
                    file_hasher.update(&buffer[..read]);
                    output
                        .write_all(&buffer[..read])
                        .map_err(|_| io_failure())?;
                }
                let after_open = file_stat(&input)?;
                let after_name = stat_at(source.as_raw_fd(), &name_c)?;
                ensure_same_file(&opened, &after_open)?;
                ensure_same_file(&opened, &after_name)?;
                if file_bytes != before.st_size as u64 {
                    return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
                }
                output.sync_all().map_err(|_| io_failure())?;
                let executable = before.st_mode & 0o111 != 0;
                fs::set_permissions(&target, fs::Permissions::from_mode(0o400))
                    .map_err(|_| io_failure())?;
                accumulator.byte_count = accumulator
                    .byte_count
                    .checked_add(file_bytes)
                    .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge))?;
                accumulator.file_count += 1;
                push_manifest_entry(
                    accumulator,
                    SnapshotEntry {
                        path: relative,
                        kind: SnapshotEntryKind::File,
                        byte_count: file_bytes,
                        sha256: Some(encode_hex(&file_hasher.finalize())),
                        executable,
                    },
                )?;
            }
            _ => return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace)),
        }
    }
    let after_directory = file_stat(source)?;
    ensure_same_file(&before_directory, &after_directory)?;
    Ok(())
}

fn push_manifest_entry(
    accumulator: &mut SnapshotAccumulator,
    entry: SnapshotEntry,
) -> Result<(), ExecutorError> {
    if entry.path.len() > 4096 || accumulator.entries.len() >= MAX_SNAPSHOT_ENTRIES {
        return Err(ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge));
    }
    let estimated = entry.path.len().saturating_mul(6).saturating_add(256);
    accumulator.estimated_manifest_bytes = accumulator
        .estimated_manifest_bytes
        .saturating_add(estimated);
    if accumulator.estimated_manifest_bytes > MAX_MANIFEST_BYTES as usize {
        return Err(ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge));
    }
    accumulator.entries.push(entry);
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn open_directory(path: &Path) -> Result<File, ExecutorError> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
pub(crate) fn openat_directory(parent: i32, name: &std::ffi::CStr) -> Result<File, ExecutorError> {
    use std::os::fd::FromRawFd;
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
pub(crate) fn openat_file(parent: i32, name: &std::ffi::CStr) -> Result<File, ExecutorError> {
    use std::os::fd::FromRawFd;
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
pub(crate) fn stat_at(parent: i32, name: &std::ffi::CStr) -> Result<libc::stat, ExecutorError> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstatat(parent, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    Ok(stat)
}

#[cfg(target_os = "linux")]
fn file_stat(file: &File) -> Result<libc::stat, ExecutorError> {
    use std::os::fd::AsRawFd;
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(file.as_raw_fd(), &mut stat) } != 0 {
        return Err(io_failure());
    }
    Ok(stat)
}

#[cfg(target_os = "linux")]
fn ensure_same_file(left: &libc::stat, right: &libc::stat) -> Result<(), ExecutorError> {
    if left.st_dev != right.st_dev
        || left.st_ino != right.st_ino
        || left.st_mode != right.st_mode
        || left.st_nlink != right.st_nlink
        || left.st_size != right.st_size
        || left.st_mtime != right.st_mtime
        || left.st_mtime_nsec != right.st_mtime_nsec
        || left.st_ctime != right.st_ctime
        || left.st_ctime_nsec != right.st_ctime_nsec
    {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::process::Command;

    use super::*;
    use crate::model::{CaptureRequest, Operation, PrepareRequest, SealRequest, TrustedConfig};

    fn git(home: &Path, directory: &Path, arguments: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/local/bin")
            .env("HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .current_dir(directory)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn git_sync_fixture() -> (tempfile::TempDir, TrustedConfig, String) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let remote = temp.path().join("remote.git");
        fs::create_dir(&remote).unwrap();
        git(&home, &remote, &["init", "--bare", "--initial-branch=main"]);
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        git(&home, &source, &["init", "--initial-branch=main"]);
        git(&home, &source, &["config", "user.name", "Artifact Fixture"]);
        git(
            &home,
            &source,
            &["config", "user.email", "artifact-fixture@example.invalid"],
        );
        fs::write(source.join("README.md"), "approved base\n").unwrap();
        git(&home, &source, &["add", "README.md"]);
        git(&home, &source, &["commit", "-m", "approved base"]);
        let base_commit = String::from_utf8(git(&home, &source, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned();
        git(
            &home,
            &source,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&home, &source, &["push", "origin", "main"]);

        let roots: Vec<PathBuf> = ["workspaces", "publishers", "state", "credentials"]
            .into_iter()
            .map(|name| {
                let path = temp.path().join(name);
                fs::create_dir(&path).unwrap();
                path
            })
            .collect();
        let repository = crate::RepositoryId {
            host: "forge.local".to_owned(),
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        };
        let config = TrustedConfig {
            protocol_version: crate::model::PROTOCOL_VERSION,
            expected_executor_version: crate::model::EXECUTOR_VERSION.to_owned(),
            workspace_root: roots[0].clone(),
            publisher_root: roots[1].clone(),
            state_root: roots[2].clone(),
            credential_root: roots[3].clone(),
            max_import_bytes: 1024 * 1024,
            repositories: vec![crate::RepositoryConfig {
                id: repository,
                clone_url: url::Url::from_file_path(&remote).unwrap().to_string(),
                ssh_directory: None,
                allowed_base_branches: vec!["main".to_owned()],
                read_credential_source: None,
                publish_credential_source: None,
                forge: None,
            }],
            credential_sources: Vec::new(),
        };
        (temp, config, base_commit)
    }

    #[test]
    fn directory_names_share_entry_budget_across_recursion_levels() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("a")).unwrap();
        fs::create_dir(root.join("b")).unwrap();
        fs::write(root.join("a/first"), b"").unwrap();
        fs::write(root.join("a/second"), b"").unwrap();
        fs::write(root.join("b/third"), b"").unwrap();

        let root_dir = open_directory(root).unwrap();
        let mut budget = SnapshotBudget::with_limits(SnapshotBudgetLimits {
            max_entries: 3,
            max_name_bytes: 64,
            max_path_bytes: 64,
        });
        let root_names = read_directory_names(&root_dir, "", true, &mut budget).unwrap();
        assert_eq!(root_names.len(), 2);

        let a_name = std::ffi::CString::new("a").unwrap();
        let a_dir = openat_directory(root_dir.as_raw_fd(), &a_name).unwrap();
        let error = read_directory_names(&a_dir, "a", false, &mut budget).unwrap_err();
        assert_eq!(error.code, ExecutorErrorCode::ArtifactTooLarge);
        assert_eq!(budget.entry_count, 3);
        assert_eq!(budget.name_bytes, 7);
    }

    #[test]
    fn directory_names_share_aggregate_path_budget_before_collection() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("a")).unwrap();
        fs::create_dir(root.join("b")).unwrap();
        fs::write(root.join("a/long-child-name"), b"").unwrap();

        let root_dir = open_directory(root).unwrap();
        let mut budget = SnapshotBudget::with_limits(SnapshotBudgetLimits {
            max_entries: 10,
            max_name_bytes: 64,
            max_path_bytes: 8,
        });
        read_directory_names(&root_dir, "", true, &mut budget).unwrap();

        let a_name = std::ffi::CString::new("a").unwrap();
        let a_dir = openat_directory(root_dir.as_raw_fd(), &a_name).unwrap();
        let error = read_directory_names(&a_dir, "a", false, &mut budget).unwrap_err();
        assert_eq!(error.code, ExecutorErrorCode::ArtifactTooLarge);
        assert_eq!(budget.path_bytes, 2);
        assert_eq!(budget.entry_count, 2);
    }

    #[test]
    fn directory_names_share_aggregate_name_budget_across_levels() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("a")).unwrap();
        fs::create_dir(root.join("b")).unwrap();
        fs::write(root.join("a/first"), b"").unwrap();

        let root_dir = open_directory(root).unwrap();
        let mut budget = SnapshotBudget::with_limits(SnapshotBudgetLimits {
            max_entries: 10,
            max_name_bytes: 6,
            max_path_bytes: 64,
        });
        read_directory_names(&root_dir, "", true, &mut budget).unwrap();

        let a_name = std::ffi::CString::new("a").unwrap();
        let a_dir = openat_directory(root_dir.as_raw_fd(), &a_name).unwrap();
        let error = read_directory_names(&a_dir, "a", false, &mut budget).unwrap_err();
        assert_eq!(error.code, ExecutorErrorCode::ArtifactTooLarge);
        assert_eq!(budget.name_bytes, 2);
        assert_eq!(budget.entry_count, 2);
    }

    #[tokio::test]
    async fn failed_git_storage_sync_never_publishes_a_seal_receipt() {
        let (_temp, config, base_commit) = git_sync_fixture();
        let validated = crate::validate_config(config.clone()).unwrap();
        let executor = crate::Executor::new(config.clone()).unwrap();
        let run_id = "sync-barrier";
        let request = crate::ExecutorRequest {
            protocol_version: crate::model::PROTOCOL_VERSION,
            operation: Operation::Prepare(PrepareRequest {
                issue_id: None,
                run_id: run_id.to_owned(),
                attempt_id: run_id.to_owned(),
                repository: config.repositories[0].id.clone(),
                base_branch: "main".to_owned(),
                base_commit: base_commit.clone(),
            }),
        };
        let prepared = executor.execute(request).await.unwrap();
        let crate::ExecutorResult::Prepared(prepared) = prepared else {
            panic!("expected prepared workspace")
        };
        fs::write(
            Path::new(&prepared.workspace_path).join("result.txt"),
            "durable\n",
        )
        .unwrap();
        let captured = executor
            .execute(crate::ExecutorRequest {
                protocol_version: crate::model::PROTOCOL_VERSION,
                operation: Operation::Capture(CaptureRequest {
                    run_id: run_id.to_owned(),
                    attempt_id: run_id.to_owned(),
                    repository: config.repositories[0].id.clone(),
                    base_branch: "main".to_owned(),
                    base_commit,
                    approved_snapshot_digest: "a".repeat(64),
                    scope_digest: "b".repeat(64),
                    intent_approval_digest: "c".repeat(64),
                }),
            })
            .await
            .unwrap();
        let crate::ExecutorResult::Captured(captured) = captured else {
            panic!("expected captured artifact")
        };
        let seal_request = SealRequest {
            run_id: run_id.to_owned(),
            attempt_id: run_id.to_owned(),
            manifest_digest: captured.manifest_digest,
        };
        let attempt_dir = config
            .state_root
            .join("runs")
            .join(run_id)
            .join("attempts")
            .join(run_id);
        let mut barrier_was_reached = false;
        let error = seal_with_git_sync(&validated, &seal_request, |_| {
            barrier_was_reached = true;
            Err(io_failure())
        })
        .unwrap_err();
        assert_eq!(error.code, ExecutorErrorCode::IoFailure);
        assert!(barrier_was_reached);
        assert!(!attempt_dir.join("seal.json").exists());

        assert!(matches!(
            executor
                .execute(crate::ExecutorRequest {
                    protocol_version: crate::model::PROTOCOL_VERSION,
                    operation: Operation::Seal(seal_request),
                })
                .await
                .unwrap(),
            crate::ExecutorResult::Sealed(_)
        ));
    }
}

#[cfg(target_os = "linux")]
fn read_directory_names(
    directory: &File,
    prefix: &str,
    root: bool,
    budget: &mut SnapshotBudget,
) -> Result<Vec<Vec<u8>>, ExecutorError> {
    use std::os::fd::AsRawFd;
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(io_failure());
    }
    let handle = unsafe { libc::fdopendir(duplicate) };
    if handle.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(io_failure());
    }
    let mut names = Vec::new();
    loop {
        unsafe { *libc::__errno_location() = 0 };
        let entry = unsafe { libc::readdir(handle) };
        if entry.is_null() {
            let error = unsafe { *libc::__errno_location() };
            unsafe { libc::closedir(handle) };
            if error != 0 {
                return Err(io_failure());
            }
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            let name_text = match std::str::from_utf8(name) {
                Ok(name_text)
                    if !name_text.is_empty()
                        && name.len() <= 255
                        && !name_text.contains('/')
                        && name_text != "."
                        && name_text != ".."
                        && prefix.matches('/').count() < 128
                        && (root || name_text != ".git") =>
                {
                    name_text
                }
                _ => {
                    unsafe { libc::closedir(handle) };
                    return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
                }
            };
            let path_len = if prefix.is_empty() {
                name_text.len()
            } else {
                let Some(path_len) = prefix
                    .len()
                    .checked_add(1)
                    .and_then(|length| length.checked_add(name_text.len()))
                else {
                    unsafe { libc::closedir(handle) };
                    return Err(ExecutorError::new(ExecutorErrorCode::ArtifactTooLarge));
                };
                path_len
            };
            if let Err(error) = budget.record_entry(name.len(), path_len) {
                unsafe { libc::closedir(handle) };
                return Err(error);
            }
            names.push(name.to_vec());
        }
    }
    Ok(names)
}
