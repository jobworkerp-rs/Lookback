//! Crash-safe tracking for manager requests whose response was not observed.
//!
//! The journal deliberately stores the encoded request bytes and the
//! idempotency key together.  A retry therefore cannot accidentally rebuild a
//! request with a new timestamp, correlation id, or actor audit.

use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io,
    path::PathBuf,
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingOperationRecord {
    pub operation_key: String,
    pub method: String,
    pub issue_id: String,
    /// Hex encoding keeps the journal self-contained without depending on a
    /// second binary codec; the name is explicit so readers do not mistake it
    /// for base64.
    pub payload_hex: String,
    pub payload_digest: String,
    pub schema_fingerprint: String,
    pub data_root_id: String,
    pub contract_fingerprint: String,
    pub created_at_ms: i64,
}

impl PendingOperationRecord {
    pub fn new(
        operation_key: impl Into<String>,
        method: impl Into<String>,
        issue_id: impl Into<String>,
        payload: &[u8],
        created_at_ms: i64,
        identity: (String, String, String),
    ) -> Self {
        let mut digest = Sha256::new();
        digest.update(payload);
        Self {
            operation_key: operation_key.into(),
            method: method.into(),
            issue_id: issue_id.into(),
            payload_hex: encode_hex(payload),
            payload_digest: format!("{:x}", digest.finalize()),
            schema_fingerprint: identity.0,
            data_root_id: identity.1,
            contract_fingerprint: identity.2,
            created_at_ms,
        }
    }

    pub fn payload(&self) -> io::Result<Vec<u8>> {
        let payload = decode_hex(&self.payload_hex)?;
        let mut digest = Sha256::new();
        digest.update(&payload);
        if format!("{:x}", digest.finalize()) != self.payload_digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "payload digest mismatch",
            ));
        }
        Ok(payload)
    }

    /// Returns whether two records describe the same request.  The creation
    /// timestamp is local journal metadata and may differ when a caller
    /// recreates a record for a safe retry after a transport error.
    fn same_request(&self, other: &Self) -> bool {
        self.operation_key == other.operation_key
            && self.method == other.method
            && self.issue_id == other.issue_id
            && self.payload_hex == other.payload_hex
            && self.payload_digest == other.payload_digest
            && self.schema_fingerprint == other.schema_fingerprint
            && self.data_root_id == other.data_root_id
            && self.contract_fingerprint == other.contract_fingerprint
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(value: &str) -> io::Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "odd length payload",
        ));
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct PendingOperationJournal {
    path: PathBuf,
    lock_path: PathBuf,
}

impl PendingOperationJournal {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_path = path.with_extension("lock");
        Self { path, lock_path }
    }

    pub fn list(&self) -> io::Result<Vec<PendingOperationRecord>> {
        self.with_read_lock(|records| Ok(records.to_vec()))
    }

    pub fn put(&self, record: PendingOperationRecord) -> io::Result<()> {
        self.with_lock(|records| {
            if let Some(existing) = records
                .iter()
                .find(|item| item.operation_key == record.operation_key)
                && !existing.same_request(&record)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "operation key is already bound to a different request",
                ));
            }
            if !records
                .iter()
                .any(|item| item.operation_key == record.operation_key)
            {
                records.push(record);
            }
            Ok(())
        })
    }

    pub fn remove(&self, operation_key: &str) -> io::Result<()> {
        self.with_lock(|records| {
            records.retain(|item| item.operation_key != operation_key);
            Ok(())
        })
    }

    /// Remove a record only when the caller still observes the exact record
    /// that it validated. This keeps status polling from deleting a newer or
    /// unrelated request that happens to reuse the same key.
    pub fn remove_record(&self, expected: &PendingOperationRecord) -> io::Result<bool> {
        self.with_lock(|records| {
            let Some(index) = records.iter().position(|record| record == expected) else {
                return Ok(false);
            };
            records.remove(index);
            Ok(true)
        })
    }

    fn with_lock<T>(
        &self,
        operation: impl FnOnce(&mut Vec<PendingOperationRecord>) -> io::Result<T>,
    ) -> io::Result<T> {
        let lock = self.open_lock()?;
        lock.lock_exclusive()?;
        let mut records = self.read_records()?;
        let result = operation(&mut records)?;
        self.write_records(&records)?;
        lock.unlock()?;
        Ok(result)
    }

    fn with_read_lock<T>(
        &self,
        operation: impl FnOnce(&[PendingOperationRecord]) -> io::Result<T>,
    ) -> io::Result<T> {
        let lock = self.open_lock()?;
        lock.lock_shared()?;
        let records = self.read_records()?;
        let result = operation(&records);
        lock.unlock()?;
        result
    }

    fn open_lock(&self) -> io::Result<std::fs::File> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)
    }

    fn read_records(&self) -> io::Result<Vec<PendingOperationRecord>> {
        let records: Vec<PendingOperationRecord> = if self.path.exists() {
            let bytes = fs::read(&self.path)?;
            serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        } else {
            Vec::new()
        };
        let mut keys = HashSet::with_capacity(records.len());
        if records
            .iter()
            .any(|record| !keys.insert(record.operation_key.as_str()))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "duplicate operation key in pending journal",
            ));
        }
        Ok(records)
    }

    fn write_records(&self, records: &[PendingOperationRecord]) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(records).map_err(io::Error::other)?;
        let temp = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&temp, bytes)?;
        let sync = OpenOptions::new().read(true).open(&temp)?;
        sync.sync_all()?;
        replace_atomically(&temp, &self.path)?;
        // The file contents are durable, but the rename itself is only
        // durable after the containing directory entry is synced. Do this
        // before returning so callers never dispatch a mutation with a
        // journal record that can disappear after a crash.
        sync_parent_directory(
            self.path
                .parent()
                .ok_or_else(|| io::Error::other("pending journal path has no parent"))?,
        )
    }
}

/// Promote a fully synced sibling temp file without exposing a missing or
/// partially-written destination. Unix `rename` replaces atomically. Windows
/// requires the native ReplaceFile API once the destination already exists.
#[cfg(not(windows))]
fn replace_atomically(temp: &std::path::Path, path: &std::path::Path) -> io::Result<()> {
    fs::rename(temp, path)
}

#[cfg(windows)]
fn replace_atomically(temp: &std::path::Path, path: &std::path::Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr;

    if !path.exists() {
        return fs::rename(temp, path);
    }
    let destination = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let replacement = temp
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // REPLACEFILE_WRITE_THROUGH requests that the replacement reaches stable
    // storage before success. No backup is needed: the temp file is retained
    // on an API error and the old destination remains intact.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ReplaceFileW(
            replaced_file_name: *const u16,
            replacement_file_name: *const u16,
            backup_file_name: *const u16,
            replace_flags: u32,
            exclude: *mut core::ffi::c_void,
            reserved: *mut core::ffi::c_void,
        ) -> i32;
    }
    // SAFETY: both paths are NUL-terminated UTF-16 buffers retained for the
    // call; the nullable optional parameters are explicitly null.
    let replaced = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            replacement.as_ptr(),
            ptr::null(),
            0x0000_0001, // REPLACEFILE_WRITE_THROUGH
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn sync_parent_directory(parent: &std::path::Path) -> io::Result<()> {
    fs::File::open(parent)?.sync_all()
}

// Windows does not expose a directory handle through `std::fs::File` that
// supports fsync. The atomic rename is the strongest portable primitive here;
// the Unix implementation above commits the directory entry explicitly.
#[cfg(windows)]
fn sync_parent_directory(_: &std::path::Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_round_trips_encoded_payload_and_rejects_key_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let journal = PendingOperationJournal::new(dir.path().join("pending.json"));
        let first = PendingOperationRecord::new(
            "key",
            "method",
            "issue",
            b"wire",
            1,
            ("schema".into(), "root".into(), "contract".into()),
        );
        journal.put(first.clone()).unwrap();
        assert_eq!(journal.list().unwrap(), vec![first.clone()]);
        assert_eq!(first.payload().unwrap(), b"wire");
        let same_request = PendingOperationRecord::new(
            "key",
            "method",
            "issue",
            b"wire",
            2,
            ("schema".into(), "root".into(), "contract".into()),
        );
        journal.put(same_request).unwrap();
        assert_eq!(journal.list().unwrap(), vec![first.clone()]);
        let second = PendingOperationRecord::new(
            "key",
            "method",
            "issue",
            b"other",
            1,
            ("schema".into(), "root".into(), "contract".into()),
        );
        assert!(journal.put(second).is_err());
        journal.remove("key").unwrap();
        assert!(journal.list().unwrap().is_empty());
    }

    #[test]
    fn listing_an_empty_journal_does_not_create_or_rewrite_the_data_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.json");
        let journal = PendingOperationJournal::new(&path);
        assert!(!path.exists());
        assert!(journal.list().unwrap().is_empty());
        assert!(!path.exists());

        let record = PendingOperationRecord::new(
            "key",
            "method",
            "issue",
            b"wire",
            1,
            ("schema".into(), "root".into(), "contract".into()),
        );
        journal.put(record).unwrap();
        let before = fs::read(&path).unwrap();
        assert_eq!(journal.list().unwrap().len(), 1);
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}
