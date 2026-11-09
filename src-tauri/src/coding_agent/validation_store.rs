use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::fs::OpenOptions;

#[cfg(all(test, unix))]
use std::cell::RefCell;

use serde::{
    Deserialize, Serialize,
    de::{DeserializeOwned, MapAccess, SeqAccess, Visitor},
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::validation::{
    ApprovedValidationPlan, MAX_VALIDATION_OUTPUT_BYTES, MAX_VALIDATION_RECORD_BYTES,
    PersistedValidationRecord, ValidationOutcome, ValidationOutputContext,
    ValidationOutputEvidence, ValidationOutputStore, ValidationOutputStoreError, ValidationReceipt,
    ValidationRecord, ValidationRestoreError, find_reusable_validation,
    output_reference_for_binding, persisted_output_evidence, persisted_validation_record,
    restore_validation_receipt, restore_validation_record,
};

const STORE_SCHEMA_VERSION: u32 = 1;
const MAX_STORE_METADATA_BYTES: usize = 32 * 1024;
const MAX_RECORDS_PER_VALIDATION: usize = 128;
const OUTPUT_REFERENCE_PREFIX: &str = "validation-output:v1:";

/// Host-native immutable storage for SANDBOX validation output and records.
///
/// The root must be an existing private directory outside every guest-writable
/// root. No path or filename is derived directly from agent-controlled data.
pub struct ValidationEvidenceStore {
    root: PathBuf,
}

impl std::fmt::Debug for ValidationEvidenceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidationEvidenceStore")
            .field("root", &"<host-private>")
            .finish_non_exhaustive()
    }
}

impl ValidationEvidenceStore {
    /// Open a pre-created private root. The adapter must pass every effective
    /// guest-writable mount root, including aliases exposed through bind mounts.
    /// Canonical path ancestry checks cannot detect filesystem mount aliases,
    /// so the adapter must also keep the evidence root out of guest mounts.
    /// Same-UID native writers are outside the guest-isolation boundary.
    pub fn open(
        root: &Path,
        guest_writable_roots: &[PathBuf],
    ) -> Result<Self, ValidationEvidenceStoreError> {
        let root = validate_directory(root, true)?;
        let guest_roots = guest_writable_roots
            .iter()
            .map(|path| validate_directory(path, false))
            .collect::<Result<Vec<_>, _>>()?;
        if guest_roots
            .iter()
            .any(|guest_root| root.starts_with(guest_root) || guest_root.starts_with(&root))
        {
            return Err(ValidationEvidenceStoreError::GuestRootOverlap);
        }

        let outputs = root.join("outputs");
        let records = root.join("records");
        match (
            path_exists_no_follow(&outputs)?,
            path_exists_no_follow(&records)?,
        ) {
            (false, false) => {
                ensure_private_directory(&root, "outputs")?;
                ensure_private_directory(&root, "records")?;
            }
            (true, true) => {
                validate_private_directory(&outputs)?;
                validate_private_directory(&records)?;
                sync_directory(&outputs)?;
                sync_directory(&records)?;
            }
            _ => return Err(ValidationEvidenceStoreError::CorruptEvidence),
        }
        let mut contents = fs::read_dir(&root).map_err(|_| ValidationEvidenceStoreError::Io)?;
        let mut names = BTreeSet::new();
        for entry in contents.by_ref() {
            let entry = entry.map_err(|_| ValidationEvidenceStoreError::Io)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ValidationEvidenceStoreError::CorruptEvidence)?;
            if !matches!(name.as_str(), "outputs" | "records") || !names.insert(name) {
                return Err(ValidationEvidenceStoreError::CorruptEvidence);
            }
        }
        if names.len() != 2 {
            return Err(ValidationEvidenceStoreError::CorruptEvidence);
        }
        sync_directory(&root)?;
        Ok(Self { root })
    }

    /// Persist one immutable host-issued record. If present, its receipt is
    /// committed only after the record and referenced output are durable.
    pub fn persist_validation_record(
        &self,
        record: &ValidationRecord,
    ) -> Result<(), ValidationEvidenceStoreError> {
        validate_record_for_storage(record)?;
        let receipt_bytes = if let Some(receipt) = record.receipt() {
            self.verify_receipt_output(record, receipt)?;
            Some(
                serde_json::to_vec(receipt)
                    .map_err(|_| ValidationEvidenceStoreError::InvalidEvidence)?,
            )
        } else {
            None
        };
        let stored_record = persisted_validation_record(record);
        let record_bytes = serde_json::to_vec(&stored_record)
            .map_err(|_| ValidationEvidenceStoreError::InvalidEvidence)?;
        let directory = self.record_directory(
            record.run_id(),
            record.attempt_id(),
            record.validation_id(),
            record.binding_digest(),
            record.child_job_id(),
        )?;
        let scope = directory
            .parent()
            .ok_or(ValidationEvidenceStoreError::UnsafeRoot)?;
        let records_root = scope
            .parent()
            .ok_or(ValidationEvidenceStoreError::UnsafeRoot)?;
        validate_private_directory(records_root)?;
        ensure_private_path(scope)?;
        ensure_private_path(&directory)?;
        write_immutable_file(&directory, "record.json", &record_bytes)?;

        match receipt_bytes {
            Some(bytes) => write_immutable_file(&directory, "receipt.json", &bytes)?,
            None if path_exists_no_follow(&directory.join("receipt.json"))? => {
                return Err(ValidationEvidenceStoreError::CorruptEvidence);
            }
            None => {}
        }
        sync_directory(&directory)?;
        Ok(())
    }

    /// Reopen bounded immutable records for this exact approved attempt.
    /// Corrupt or incomplete receipt material is an error, never a success.
    pub fn load_validation_records(
        &self,
        plan: &ApprovedValidationPlan,
    ) -> Result<Vec<ValidationRecord>, ValidationEvidenceStoreError> {
        let scope = self.record_scope_directory(plan)?;
        match fs::symlink_metadata(&scope) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(ValidationEvidenceStoreError::Io),
            Ok(metadata) => {
                validate_private_directory_metadata(&metadata)?;
                sync_directory(&self.root.join("records"))?;
                sync_directory(&scope)?;
            }
        }

        let entries = fs::read_dir(&scope).map_err(|_| ValidationEvidenceStoreError::Io)?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|_| ValidationEvidenceStoreError::Io)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ValidationEvidenceStoreError::CorruptEvidence)?;
            if !valid_record_directory_name(&name) {
                return Err(ValidationEvidenceStoreError::CorruptEvidence);
            }
            let metadata =
                fs::symlink_metadata(entry.path()).map_err(|_| ValidationEvidenceStoreError::Io)?;
            validate_private_directory_metadata(&metadata)?;
            sync_directory(&entry.path())?;
            sync_directory(&scope)?;
            names.push(name);
            if names.len() > MAX_RECORDS_PER_VALIDATION {
                return Err(ValidationEvidenceStoreError::BoundsExceeded);
            }
        }
        names.sort();

        names
            .into_iter()
            .map(|name| self.load_record_directory(plan, &scope.join(name)))
            .collect()
    }

    /// Return only a fully verified passed receipt for this plan.
    pub fn find_reusable_validation(
        &self,
        plan: &ApprovedValidationPlan,
    ) -> Result<Option<ValidationReceipt>, ValidationEvidenceStoreError> {
        let records = self.load_validation_records(plan)?;
        let receipts = records
            .iter()
            .filter_map(ValidationRecord::receipt)
            .cloned()
            .collect::<Vec<_>>();
        Ok(find_reusable_validation(plan, &receipts).cloned())
    }

    fn load_record_directory(
        &self,
        plan: &ApprovedValidationPlan,
        directory: &Path,
    ) -> Result<ValidationRecord, ValidationEvidenceStoreError> {
        let record_bytes =
            read_safe_file(&directory.join("record.json"), MAX_VALIDATION_RECORD_BYTES)?;
        let record_value = parse_unique_json(&record_bytes)?;
        let stored: PersistedValidationRecord = decode_unique_value(record_value.clone())?;
        validate_stored_record_path(directory, &stored, plan)?;

        let receipt_path = directory.join("receipt.json");
        let receipt_file_exists = path_exists_no_follow(&receipt_path)?;
        let receipt = match (&stored.receipt_id, receipt_file_exists) {
            (Some(_), true) => {
                let job_id = stored
                    .child_job_id
                    .as_deref()
                    .ok_or(ValidationEvidenceStoreError::CorruptEvidence)?;
                let receipt_bytes = read_safe_file(&receipt_path, MAX_VALIDATION_RECORD_BYTES)?;
                let receipt_value = parse_unique_json(&receipt_bytes)?;
                let output_reference = output_reference_for_binding(plan.binding_digest(), job_id);
                let output = self.load_output(
                    &output_reference,
                    plan.run_id(),
                    plan.attempt_id(),
                    plan.validation_id(),
                    plan.binding_digest(),
                    job_id,
                )?;
                let receipt = restore_validation_receipt(receipt_value, plan, job_id, &output)
                    .map_err(map_restore_error)?;
                if receipt.receipt_id() != stored.receipt_id.as_deref().unwrap_or_default() {
                    return Err(ValidationEvidenceStoreError::CorruptEvidence);
                }
                Some(receipt)
            }
            (None, false) => None,
            _ => return Err(ValidationEvidenceStoreError::CorruptEvidence),
        };
        restore_validation_record(stored, plan, receipt).map_err(map_restore_error)
    }

    fn verify_receipt_output(
        &self,
        record: &ValidationRecord,
        receipt: &ValidationReceipt,
    ) -> Result<(), ValidationEvidenceStoreError> {
        if receipt.schema_version() != 1
            || receipt.run_id() != record.run_id()
            || receipt.attempt_id() != record.attempt_id()
            || receipt.validation_id() != record.validation_id()
            || receipt.required() != record.required()
            || receipt.binding_digest() != record.binding_digest()
            || Some(receipt.child_job_id()) != record.child_job_id()
            || Some(receipt.sandbox_id()) != record.sandbox_id()
            || Some(receipt.exit_code()) != record.exit_code()
            || Some(receipt.execution_time_ms()) != record.execution_time_ms()
            || receipt.outcome() != record.outcome()
            || !record.end_observed()
            || record.transport_error_observed()
            || !receipt.output().capture_complete()
        {
            return Err(ValidationEvidenceStoreError::InvalidEvidence);
        }
        let expected_reference =
            output_reference_for_binding(record.binding_digest(), receipt.child_job_id());
        if receipt.output().reference() != expected_reference {
            return Err(ValidationEvidenceStoreError::InvalidEvidence);
        }
        let output = self.load_output(
            &expected_reference,
            record.run_id(),
            record.attempt_id(),
            record.validation_id(),
            record.binding_digest(),
            receipt.child_job_id(),
        )?;
        if output != *receipt.output() {
            return Err(ValidationEvidenceStoreError::InvalidEvidence);
        }
        Ok(())
    }

    fn load_output(
        &self,
        reference: &str,
        run_id: &str,
        attempt_id: &str,
        validation_id: &str,
        binding_digest: &str,
        job_id: &str,
    ) -> Result<ValidationOutputEvidence, ValidationEvidenceStoreError> {
        let token = output_token(reference)?;
        if reference != output_reference_for_binding(binding_digest, job_id) {
            return Err(ValidationEvidenceStoreError::CorruptEvidence);
        }
        let directory = self.root.join("outputs").join(token);
        validate_private_directory(&directory)?;
        sync_directory(&self.root.join("outputs"))?;
        sync_directory(&directory)?;
        let manifest_bytes =
            read_safe_file(&directory.join("output.json"), MAX_STORE_METADATA_BYTES)?;
        let manifest_value = parse_unique_json(&manifest_bytes)?;
        let manifest: StoredOutputManifest = decode_unique_value(manifest_value)?;
        if manifest.schema_version != STORE_SCHEMA_VERSION
            || manifest.output_reference != reference
            || manifest.run_id != run_id
            || manifest.attempt_id != attempt_id
            || manifest.validation_id != validation_id
            || manifest.binding_digest != binding_digest
            || manifest.job_id != job_id
            || !manifest.capture_complete
            || manifest.stdout_bytes.saturating_add(manifest.stderr_bytes)
                > MAX_VALIDATION_OUTPUT_BYTES
            || !valid_digest(&manifest.stdout_sha256)
            || !valid_digest(&manifest.stderr_sha256)
        {
            return Err(ValidationEvidenceStoreError::CorruptEvidence);
        }
        let stdout = read_safe_file(&directory.join("stdout.bin"), manifest.stdout_bytes)?;
        let stderr = read_safe_file(&directory.join("stderr.bin"), manifest.stderr_bytes)?;
        if stdout.len() != manifest.stdout_bytes || stderr.len() != manifest.stderr_bytes {
            return Err(ValidationEvidenceStoreError::CorruptEvidence);
        }
        let evidence = persisted_output_evidence(reference.to_owned(), &stdout, &stderr)
            .map_err(map_restore_error)?;
        if evidence.stdout_sha256() != manifest.stdout_sha256
            || evidence.stderr_sha256() != manifest.stderr_sha256
        {
            return Err(ValidationEvidenceStoreError::CorruptEvidence);
        }
        Ok(evidence)
    }

    fn record_scope_directory(
        &self,
        plan: &ApprovedValidationPlan,
    ) -> Result<PathBuf, ValidationEvidenceStoreError> {
        let token = digest_serializable(&RecordScope {
            run_id: plan.run_id(),
            attempt_id: plan.attempt_id(),
            validation_id: plan.validation_id(),
            binding_digest: plan.binding_digest(),
        })?;
        Ok(self.root.join("records").join(token))
    }

    fn record_directory(
        &self,
        run_id: &str,
        attempt_id: &str,
        validation_id: &str,
        binding_digest: &str,
        job_id: Option<&str>,
    ) -> Result<PathBuf, ValidationEvidenceStoreError> {
        let scope = digest_serializable(&RecordScope {
            run_id,
            attempt_id,
            validation_id,
            binding_digest,
        })?;
        let entry = match job_id {
            Some(job_id) if is_canonical_job_id(job_id) => {
                format!("job-{}", digest_bytes(job_id.as_bytes()))
            }
            Some(_) => return Err(ValidationEvidenceStoreError::InvalidEvidence),
            None => format!("not-run-{}", digest_bytes(binding_digest.as_bytes())),
        };
        Ok(self.root.join("records").join(scope).join(entry))
    }

    fn persist_output(
        &self,
        context: &ValidationOutputContext<'_>,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<String, ValidationEvidenceStoreError> {
        if !is_canonical_job_id(context.job_id())
            || !valid_digest(context.binding_digest())
            || stdout.len().saturating_add(stderr.len()) > MAX_VALIDATION_OUTPUT_BYTES
        {
            return Err(ValidationEvidenceStoreError::InvalidEvidence);
        }
        let reference = output_reference_for_binding(context.binding_digest(), context.job_id());
        let token = output_token(&reference)?;
        let directory = self.root.join("outputs").join(token);
        ensure_private_path(&directory)?;
        let evidence = persisted_output_evidence(reference.clone(), stdout, stderr)
            .map_err(map_restore_error)?;
        let manifest = StoredOutputManifest {
            schema_version: STORE_SCHEMA_VERSION,
            output_reference: reference.clone(),
            run_id: context.run_id().to_owned(),
            attempt_id: context.attempt_id().to_owned(),
            validation_id: context.validation_id().to_owned(),
            binding_digest: context.binding_digest().to_owned(),
            job_id: context.job_id().to_owned(),
            stdout_sha256: evidence.stdout_sha256().to_owned(),
            stderr_sha256: evidence.stderr_sha256().to_owned(),
            stdout_bytes: evidence.stdout_bytes(),
            stderr_bytes: evidence.stderr_bytes(),
            capture_complete: true,
        };
        let manifest_bytes = serde_json::to_vec(&manifest)
            .map_err(|_| ValidationEvidenceStoreError::InvalidEvidence)?;
        write_immutable_file(&directory, "stdout.bin", stdout)?;
        write_immutable_file(&directory, "stderr.bin", stderr)?;
        write_immutable_file(&directory, "output.json", &manifest_bytes)?;
        sync_directory(&directory)?;
        let verified = self.load_output(
            &reference,
            context.run_id(),
            context.attempt_id(),
            context.validation_id(),
            context.binding_digest(),
            context.job_id(),
        )?;
        if verified != evidence {
            return Err(ValidationEvidenceStoreError::CorruptEvidence);
        }
        Ok(reference)
    }
}

impl ValidationOutputStore for ValidationEvidenceStore {
    fn persist_validation_output(
        &mut self,
        context: &ValidationOutputContext<'_>,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<String, ValidationOutputStoreError> {
        self.persist_output(context, stdout, stderr)
            .map_err(|_| ValidationOutputStoreError)
    }

    fn persist_validation_record(
        &mut self,
        record: &ValidationRecord,
    ) -> Result<(), ValidationOutputStoreError> {
        ValidationEvidenceStore::persist_validation_record(self, record)
            .map_err(|_| ValidationOutputStoreError)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ValidationEvidenceStoreError {
    #[error("validation evidence root is unsafe")]
    UnsafeRoot,
    #[error("validation evidence root overlaps guest-writable storage")]
    GuestRootOverlap,
    #[error("validation evidence is invalid")]
    InvalidEvidence,
    #[error("validation evidence storage is corrupt or incomplete")]
    CorruptEvidence,
    #[error("validation evidence exceeds a storage bound")]
    BoundsExceeded,
    #[error("validation evidence filesystem operation failed")]
    Io,
    #[error("private validation evidence storage is unsupported on this platform")]
    UnsupportedPlatform,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredOutputManifest {
    schema_version: u32,
    output_reference: String,
    run_id: String,
    attempt_id: String,
    validation_id: String,
    binding_digest: String,
    job_id: String,
    stdout_sha256: String,
    stderr_sha256: String,
    stdout_bytes: usize,
    stderr_bytes: usize,
    capture_complete: bool,
}

#[derive(Serialize)]
struct RecordScope<'a> {
    run_id: &'a str,
    attempt_id: &'a str,
    validation_id: &'a str,
    binding_digest: &'a str,
}

fn validate_record_for_storage(
    record: &ValidationRecord,
) -> Result<(), ValidationEvidenceStoreError> {
    if record.schema_version() != 1 || !valid_digest(record.binding_digest()) {
        return Err(ValidationEvidenceStoreError::InvalidEvidence);
    }
    match record.receipt() {
        Some(receipt) => {
            if record.outcome() != receipt.outcome()
                || receipt.run_id() != record.run_id()
                || receipt.attempt_id() != record.attempt_id()
                || receipt.validation_id() != record.validation_id()
                || receipt.required() != record.required()
                || receipt.binding_digest() != record.binding_digest()
                || Some(receipt.child_job_id()) != record.child_job_id()
                || Some(receipt.sandbox_id()) != record.sandbox_id()
                || Some(receipt.exit_code()) != record.exit_code()
                || Some(receipt.execution_time_ms()) != record.execution_time_ms()
                || !record.end_observed()
                || record.transport_error_observed()
                || receipt.schema_version() != 1
            {
                return Err(ValidationEvidenceStoreError::InvalidEvidence);
            }
        }
        None if record.outcome() == ValidationOutcome::Passed => {
            return Err(ValidationEvidenceStoreError::InvalidEvidence);
        }
        None => {}
    }
    if record
        .child_job_id()
        .is_some_and(|job_id| !is_canonical_job_id(job_id))
    {
        return Err(ValidationEvidenceStoreError::InvalidEvidence);
    }
    Ok(())
}

fn validate_stored_record_path(
    directory: &Path,
    record: &PersistedValidationRecord,
    plan: &ApprovedValidationPlan,
) -> Result<(), ValidationEvidenceStoreError> {
    let directory_name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ValidationEvidenceStoreError::CorruptEvidence)?;
    let expected_name = match record.child_job_id.as_deref() {
        Some(job_id) if is_canonical_job_id(job_id) => {
            format!("job-{}", digest_bytes(job_id.as_bytes()))
        }
        Some(_) => return Err(ValidationEvidenceStoreError::CorruptEvidence),
        None => format!("not-run-{}", digest_bytes(plan.binding_digest().as_bytes())),
    };
    if directory_name != expected_name
        || record.run_id != plan.run_id()
        || record.attempt_id != plan.attempt_id()
        || record.validation_id != plan.validation_id()
        || record.binding_digest != plan.binding_digest()
    {
        return Err(ValidationEvidenceStoreError::CorruptEvidence);
    }
    Ok(())
}

fn output_token(reference: &str) -> Result<String, ValidationEvidenceStoreError> {
    let token = reference
        .strip_prefix(OUTPUT_REFERENCE_PREFIX)
        .ok_or(ValidationEvidenceStoreError::InvalidEvidence)?;
    if !valid_digest(token) {
        return Err(ValidationEvidenceStoreError::InvalidEvidence);
    }
    Ok(token.to_owned())
}

fn is_canonical_job_id(job_id: &str) -> bool {
    job_id
        .parse::<i64>()
        .is_ok_and(|id| id > 0 && id.to_string() == job_id)
}

fn valid_record_directory_name(name: &str) -> bool {
    name.strip_prefix("job-")
        .or_else(|| name.strip_prefix("not-run-"))
        .is_some_and(valid_digest)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn digest_serializable(value: &impl Serialize) -> Result<String, ValidationEvidenceStoreError> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| ValidationEvidenceStoreError::InvalidEvidence)?;
    Ok(digest_bytes(&bytes))
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn map_restore_error(_: ValidationRestoreError) -> ValidationEvidenceStoreError {
    ValidationEvidenceStoreError::CorruptEvidence
}

fn validate_directory(
    path: &Path,
    require_private: bool,
) -> Result<PathBuf, ValidationEvidenceStoreError> {
    if !path.is_absolute() {
        return Err(ValidationEvidenceStoreError::UnsafeRoot);
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| ValidationEvidenceStoreError::UnsafeRoot)?;
    if canonical != path {
        return Err(ValidationEvidenceStoreError::UnsafeRoot);
    }
    validate_no_symlink_components(&canonical)?;
    let metadata =
        fs::symlink_metadata(&canonical).map_err(|_| ValidationEvidenceStoreError::UnsafeRoot)?;
    if !metadata.file_type().is_dir() {
        return Err(ValidationEvidenceStoreError::UnsafeRoot);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let owner = unsafe { libc::geteuid() };
        if metadata.uid() != owner
            || (require_private && metadata.mode() & 0o077 != 0)
            || (require_private && metadata.mode() & 0o700 != 0o700)
        {
            return Err(ValidationEvidenceStoreError::UnsafeRoot);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = require_private;
        return Err(ValidationEvidenceStoreError::UnsupportedPlatform);
    }
    Ok(canonical)
}

fn validate_no_symlink_components(path: &Path) -> Result<(), ValidationEvidenceStoreError> {
    let mut current = PathBuf::new();
    let components = path.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| ValidationEvidenceStoreError::UnsafeRoot)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            return Err(ValidationEvidenceStoreError::UnsafeRoot);
        }
        #[cfg(not(unix))]
        let _ = index;
        #[cfg(unix)]
        if index + 1 < components.len() {
            use std::os::unix::fs::MetadataExt;
            let mode = metadata.mode();
            let sticky = mode & 0o1000 != 0;
            if mode & 0o022 != 0 && !sticky {
                return Err(ValidationEvidenceStoreError::UnsafeRoot);
            }
        }
    }
    Ok(())
}

fn ensure_private_directory(
    parent: &Path,
    name: &str,
) -> Result<PathBuf, ValidationEvidenceStoreError> {
    let directory = parent.join(name);
    match fs::symlink_metadata(&directory) {
        Ok(metadata) => validate_private_directory_metadata(&metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_directory_0700(&directory)?;
            let metadata =
                fs::symlink_metadata(&directory).map_err(|_| ValidationEvidenceStoreError::Io)?;
            validate_private_directory_metadata(&metadata)?;
        }
        Err(_) => return Err(ValidationEvidenceStoreError::Io),
    }
    sync_directory(&directory)?;
    sync_directory(parent)?;
    Ok(directory)
}

fn ensure_private_path(path: &Path) -> Result<(), ValidationEvidenceStoreError> {
    let parent = path
        .parent()
        .ok_or(ValidationEvidenceStoreError::UnsafeRoot)?;
    validate_private_directory(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_directory_metadata(&metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_directory_0700(path)?;
        }
        Err(_) => return Err(ValidationEvidenceStoreError::Io),
    }
    sync_directory(path)?;
    sync_directory(parent)
}

fn create_directory_0700(path: &Path) -> Result<(), ValidationEvidenceStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(path)
            .map_err(|_| ValidationEvidenceStoreError::Io)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(ValidationEvidenceStoreError::UnsupportedPlatform)
    }
}

fn validate_private_directory(path: &Path) -> Result<(), ValidationEvidenceStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ValidationEvidenceStoreError::Io)?;
    validate_private_directory_metadata(&metadata)
}

fn validate_private_directory_metadata(
    metadata: &fs::Metadata,
) -> Result<(), ValidationEvidenceStoreError> {
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(ValidationEvidenceStoreError::CorruptEvidence);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.mode() & 0o700 != 0o700
        {
            return Err(ValidationEvidenceStoreError::UnsafeRoot);
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), ValidationEvidenceStoreError> {
    #[cfg(unix)]
    {
        let directory = open_directory_nofollow(path)?;
        let opened_metadata = directory
            .metadata()
            .map_err(|_| ValidationEvidenceStoreError::Io)?;
        validate_private_directory_metadata(&opened_metadata)?;
        let path_metadata =
            fs::symlink_metadata(path).map_err(|_| ValidationEvidenceStoreError::UnsafeRoot)?;
        validate_private_directory_metadata(&path_metadata)?;
        use std::os::unix::fs::MetadataExt;
        if opened_metadata.dev() != path_metadata.dev()
            || opened_metadata.ino() != path_metadata.ino()
        {
            return Err(ValidationEvidenceStoreError::UnsafeRoot);
        }
        #[cfg(test)]
        invoke_directory_sync_hook(path)?;
        directory
            .sync_all()
            .map_err(|_| ValidationEvidenceStoreError::Io)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(ValidationEvidenceStoreError::UnsupportedPlatform)
    }
}

#[cfg(unix)]
fn open_directory_nofollow(path: &Path) -> Result<File, ValidationEvidenceStoreError> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .map_err(|_| ValidationEvidenceStoreError::UnsafeRoot)
}

#[cfg(all(test, unix))]
type DirectorySyncHook = Box<dyn Fn(&Path) -> Result<(), ValidationEvidenceStoreError> + 'static>;

#[cfg(all(test, unix))]
std::thread_local! {
    static DIRECTORY_SYNC_HOOK: RefCell<Option<DirectorySyncHook>> = RefCell::new(None);
}

#[cfg(all(test, unix))]
struct DirectorySyncHookGuard;

#[cfg(all(test, unix))]
impl Drop for DirectorySyncHookGuard {
    fn drop(&mut self) {
        DIRECTORY_SYNC_HOOK.with(|hook| *hook.borrow_mut() = None);
    }
}

#[cfg(all(test, unix))]
fn install_directory_sync_hook(
    hook: impl Fn(&Path) -> Result<(), ValidationEvidenceStoreError> + 'static,
) -> DirectorySyncHookGuard {
    DIRECTORY_SYNC_HOOK.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "directory sync hook already installed"
        );
        *slot.borrow_mut() = Some(Box::new(hook));
    });
    DirectorySyncHookGuard
}

#[cfg(all(test, unix))]
fn invoke_directory_sync_hook(path: &Path) -> Result<(), ValidationEvidenceStoreError> {
    DIRECTORY_SYNC_HOOK.with(|slot| match slot.borrow().as_ref() {
        Some(hook) => hook(path),
        None => Ok(()),
    })
}

fn write_immutable_file(
    directory: &Path,
    filename: &str,
    bytes: &[u8],
) -> Result<(), ValidationEvidenceStoreError> {
    validate_private_directory(directory)?;
    let destination = directory.join(filename);
    if path_exists_no_follow(&destination)? {
        let existing = read_safe_file(&destination, bytes.len())?;
        if existing != bytes {
            return Err(ValidationEvidenceStoreError::CorruptEvidence);
        }
        return Ok(());
    }

    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| ValidationEvidenceStoreError::Io)?;
    let temporary = directory.join(format!(".stage-{}", hex(&random)));
    let mut file = open_new_private_file(&temporary)?;
    let write_result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err(ValidationEvidenceStoreError::Io);
    }
    match fs::hard_link(&temporary, &destination) {
        Ok(()) => sync_directory(directory)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = read_safe_file(&destination, bytes.len())?;
            if existing != bytes {
                let _ = fs::remove_file(&temporary);
                return Err(ValidationEvidenceStoreError::CorruptEvidence);
            }
        }
        Err(_) => {
            let _ = fs::remove_file(&temporary);
            return Err(ValidationEvidenceStoreError::Io);
        }
    }
    fs::remove_file(&temporary).map_err(|_| ValidationEvidenceStoreError::Io)?;
    sync_directory(directory)
}

fn open_new_private_file(path: &Path) -> Result<File, ValidationEvidenceStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|_| ValidationEvidenceStoreError::Io)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(ValidationEvidenceStoreError::UnsupportedPlatform)
    }
}

fn read_safe_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>, ValidationEvidenceStoreError> {
    let before =
        fs::symlink_metadata(path).map_err(|_| ValidationEvidenceStoreError::CorruptEvidence)?;
    validate_private_file_metadata(&before)?;
    if before.len() > max_bytes as u64 {
        return Err(ValidationEvidenceStoreError::BoundsExceeded);
    }
    let file = open_readonly_nofollow(path)?;
    let after = file
        .metadata()
        .map_err(|_| ValidationEvidenceStoreError::Io)?;
    validate_private_file_metadata(&after)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(ValidationEvidenceStoreError::CorruptEvidence);
        }
    }
    let mut captured = Vec::with_capacity(before.len() as usize);
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut captured)
        .map_err(|_| ValidationEvidenceStoreError::Io)?;
    if captured.len() > max_bytes || captured.len() as u64 != before.len() {
        return Err(ValidationEvidenceStoreError::CorruptEvidence);
    }
    Ok(captured)
}

fn open_readonly_nofollow(path: &Path) -> Result<File, ValidationEvidenceStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| ValidationEvidenceStoreError::CorruptEvidence)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(ValidationEvidenceStoreError::UnsupportedPlatform)
    }
}

fn validate_private_file_metadata(
    metadata: &fs::Metadata,
) -> Result<(), ValidationEvidenceStoreError> {
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(ValidationEvidenceStoreError::CorruptEvidence);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(ValidationEvidenceStoreError::UnsafeRoot);
        }
    }
    Ok(())
}

fn path_exists_no_follow(path: &Path) -> Result<bool, ValidationEvidenceStoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(ValidationEvidenceStoreError::Io),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn parse_unique_json(bytes: &[u8]) -> Result<serde_json::Value, ValidationEvidenceStoreError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let UniqueJson(value) = UniqueJson::deserialize(&mut deserializer)
        .map_err(|_| ValidationEvidenceStoreError::CorruptEvidence)?;
    deserializer
        .end()
        .map_err(|_| ValidationEvidenceStoreError::CorruptEvidence)?;
    Ok(value)
}

fn decode_unique_value<T: DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, ValidationEvidenceStoreError> {
    serde_json::from_value(value).map_err(|_| ValidationEvidenceStoreError::CorruptEvidence)
}

struct UniqueJson(serde_json::Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueJsonVisitor)
    }
}

struct UniqueJsonVisitor;

impl<'de> Visitor<'de> for UniqueJsonVisitor {
    type Value = UniqueJson;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JSON with unique object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueJson(serde_json::Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueJson(serde_json::Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueJson(serde_json::Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .map(UniqueJson)
            .ok_or_else(|| E::custom("invalid JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueJson(serde_json::Value::String(value.to_owned())))
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_str(value)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueJson(serde_json::Value::String(value)))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueJson(serde_json::Value::Null))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_unit()
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        UniqueJson::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(UniqueJson(value)) = sequence.next_element()? {
            values.push(value);
        }
        Ok(UniqueJson(serde_json::Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen = BTreeSet::new();
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(serde::de::Error::custom("duplicate JSON object key"));
            }
            let UniqueJson(value) = map.next_value()?;
            values.insert(key, value);
        }
        Ok(UniqueJson(serde_json::Value::Object(values)))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{collections::BTreeMap, sync::mpsc, thread, time::Duration};

    use super::*;
    use crate::coding_agent::validation::{ValidationTargetSnapshot, record_validation_not_run};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn private_root(temp: &TempDir) -> PathBuf {
        let root = temp.path().join("evidence");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn plan() -> ApprovedValidationPlan {
        ApprovedValidationPlan::new_for_worker(
            "run-sync-test",
            "attempt-sync-test",
            "validation-sync-test",
            true,
            "sandbox-worker",
            vec!["cargo".to_owned(), "test".to_owned()],
            "/workspace/source",
            BTreeMap::new(),
            "d".repeat(64),
            Some(30_000),
            ValidationTargetSnapshot::new("a".repeat(40), "b".repeat(40), "c".repeat(64)).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn existing_scope_sync_failure_aborts_record_commit() {
        let temp = TempDir::new().unwrap();
        let root = private_root(&temp);
        let store = ValidationEvidenceStore::open(&root, &[]).unwrap();
        let approved = plan();
        let record = record_validation_not_run(&approved);
        let scope = store.record_scope_directory(&approved).unwrap();
        ensure_private_path(&scope).unwrap();
        let record_directory = store
            .record_directory(
                record.run_id(),
                record.attempt_id(),
                record.validation_id(),
                record.binding_digest(),
                record.child_job_id(),
            )
            .unwrap();

        let records_root = root.join("records");
        let _hook = install_directory_sync_hook(move |path| {
            if path == records_root {
                Err(ValidationEvidenceStoreError::Io)
            } else {
                Ok(())
            }
        });

        assert_eq!(
            store.persist_validation_record(&record),
            Err(ValidationEvidenceStoreError::Io)
        );
        assert!(!record_directory.join("record.json").exists());
        assert!(!record_directory.join("receipt.json").exists());
    }

    #[test]
    fn directory_sync_does_not_follow_a_symlink_path() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target");
        let alias = temp.path().join("alias");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, &alias).unwrap();

        assert_eq!(
            sync_directory(&alias),
            Err(ValidationEvidenceStoreError::UnsafeRoot)
        );
    }

    #[test]
    fn concurrent_store_persist_fsyncs_a_published_scope_before_acknowledging() {
        let temp = TempDir::new().unwrap();
        let root = private_root(&temp);
        drop(ValidationEvidenceStore::open(&root, &[]).unwrap());
        let approved = plan();
        let record = record_validation_not_run(&approved);
        let records_root = root.join("records");

        let (first_scope_created_tx, first_scope_created_rx) = mpsc::channel();
        let (release_first_sync_tx, release_first_sync_rx) = mpsc::channel();
        let first_root = root.clone();
        let first_record = record.clone();
        let first_records_root = records_root.clone();
        let first = thread::spawn(move || {
            let store = ValidationEvidenceStore::open(&first_root, &[]).unwrap();
            let _hook = install_directory_sync_hook(move |path| {
                if path == first_records_root {
                    first_scope_created_tx.send(()).unwrap();
                    release_first_sync_rx
                        .recv()
                        .map_err(|_| ValidationEvidenceStoreError::Io)?;
                }
                Ok(())
            });
            store.persist_validation_record(&first_record)
        });

        if first_scope_created_rx
            .recv_timeout(Duration::from_secs(3))
            .is_err()
        {
            let _ = release_first_sync_tx.send(());
            let _ = first.join();
            panic!("first writer did not reach the parent-directory sync barrier");
        }

        let (second_parent_sync_tx, second_parent_sync_rx) = mpsc::channel();
        let (release_second_sync_tx, release_second_sync_rx) = mpsc::channel();
        let (second_done_tx, second_done_rx) = mpsc::channel();
        let second_root = root.clone();
        let second_record = record.clone();
        let second_records_root = records_root.clone();
        let second = thread::spawn(move || {
            let store = ValidationEvidenceStore::open(&second_root, &[]).unwrap();
            let _hook = install_directory_sync_hook(move |path| {
                if path == second_records_root {
                    second_parent_sync_tx.send(()).unwrap();
                    release_second_sync_rx
                        .recv()
                        .map_err(|_| ValidationEvidenceStoreError::Io)?;
                }
                Ok(())
            });
            let result = store.persist_validation_record(&second_record);
            second_done_tx.send(result).unwrap();
        });

        let second_reached_parent_sync = second_parent_sync_rx
            .recv_timeout(Duration::from_secs(3))
            .is_ok();
        let second_acknowledged_early = second_done_rx.try_recv().ok();
        let _ = release_second_sync_tx.send(());
        let second_result = second_acknowledged_early
            .or_else(|| second_done_rx.recv_timeout(Duration::from_secs(3)).ok())
            .expect("second writer did not finish");
        second.join().unwrap();
        let _ = release_first_sync_tx.send(());
        let first_result = first.join().unwrap();

        assert!(second_reached_parent_sync);
        assert!(second_acknowledged_early.is_none());
        assert_eq!(second_result, Ok(()));
        assert_eq!(first_result, Ok(()));

        let reopened = ValidationEvidenceStore::open(&root, &[]).unwrap();
        let records = reopened.load_validation_records(&approved).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome(), ValidationOutcome::NotRun);
    }
}
