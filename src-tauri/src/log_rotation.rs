//! Rotation and retention for application-managed log files.
//!
//! Active files keep their existing names so readers can continue to tail
//! them. A writer closes the active file before renaming it, which also keeps
//! rotation safe on Windows where an open file cannot be renamed. Native
//! process logs are isolated below `native/` and are pruned only after their
//! owner marks the directory closed.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::time::Duration;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temporary log directory")
    }

    fn archive_path_at(dir: &Path, timestamp: chrono::DateTime<Utc>, sequence: u32) -> PathBuf {
        dir.join(format!(
            "lookback.log.rotated-{}-{sequence}",
            timestamp.format("%Y%m%dT%H%M%SZ")
        ))
    }

    #[test]
    fn rotates_before_a_write_that_crosses_the_size_limit() {
        let dir = temp_dir();
        let path = dir.path().join("jobworkerp.stdout.log");
        let mut writer = RotatingFile::with_max_bytes(path.clone(), 4).unwrap();
        writer.write_all(b"abcd").unwrap();
        writer.write_all(b"e").unwrap();
        writer.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"e");
        let archives = archive_paths(dir.path());
        assert_eq!(archives.len(), 1);
        assert_eq!(fs::read(&archives[0]).unwrap(), b"abcd");
    }

    #[test]
    fn rotates_when_the_utc_day_changes_even_below_size_limit() {
        let dir = temp_dir();
        let path = dir.path().join("lookback.log");
        let mut writer = RotatingFile::with_max_bytes(path.clone(), 1024).unwrap();
        writer.write_all(b"yesterday").unwrap();
        writer.day = Utc::now().date_naive() - chrono::Duration::days(1);
        writer.write_all(b"today").unwrap();
        writer.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"today");
        assert_eq!(archive_paths(dir.path()).len(), 1);
    }

    #[test]
    fn native_log_dir_is_unique_and_sanitizes_scope() {
        let dir = temp_dir();
        let first = native_log_dir(dir.path(), "memories/front").unwrap();
        let second = native_log_dir(dir.path(), "memories/front").unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with(dir.path().join("native")));
        assert!(!first.file_name().unwrap().to_string_lossy().contains('/'));
        assert!(first.is_dir());
        assert!(first.join(NATIVE_ACTIVE_MARKER).is_file());
        mark_native_log_dir_closed(&first).unwrap();
        assert!(first.join(NATIVE_CLOSED_MARKER).is_file());
        assert!(!first.join(NATIVE_ACTIVE_MARKER).exists());
        mark_native_log_dir_pid(&second, 2_000_000_000).unwrap();
        assert_eq!(reconcile_native_log_dirs_after_reap(dir.path()).unwrap(), 1);
        assert!(second.join(NATIVE_CLOSED_MARKER).is_file());
    }

    #[test]
    fn reconcile_closes_stale_native_active_dirs() {
        let dir = temp_dir();
        let stale = native_log_dir(dir.path(), "memories-front").unwrap();
        mark_native_log_dir_pid(&stale, 2_000_000_000).unwrap();
        fs::write(stale.join("Lookback.log"), b"orphaned").unwrap();

        let closed = reconcile_native_log_dirs(dir.path()).unwrap();

        assert_eq!(closed, 1);
        assert!(!stale.join(NATIVE_ACTIVE_MARKER).exists());
        assert!(stale.join(NATIVE_CLOSED_MARKER).is_file());
    }

    #[test]
    fn active_native_scope_without_recorded_child_pid_is_not_reconciled() {
        let dir = temp_dir();
        let active = native_log_dir(dir.path(), "memories-front").unwrap();
        fs::write(active.join("Lookback.log"), b"still starting").unwrap();

        assert_eq!(reconcile_native_log_dirs(dir.path()).unwrap(), 0);
        assert!(active.join(NATIVE_ACTIVE_MARKER).is_file());
        assert!(!active.join(NATIVE_CLOSED_MARKER).exists());
    }

    #[test]
    fn recorded_native_child_pid_keeps_a_live_scope_active() {
        let dir = temp_dir();
        let active = native_log_dir(dir.path(), "memories-front").unwrap();
        mark_native_log_dir_pid(&active, std::process::id()).unwrap();

        assert_eq!(
            fs::read_to_string(active.join(NATIVE_ACTIVE_MARKER)).unwrap(),
            format!("pid={}\n", std::process::id())
        );
        assert_eq!(reconcile_native_log_dirs(dir.path()).unwrap(), 0);
        assert!(active.join(NATIVE_ACTIVE_MARKER).exists());
    }

    #[test]
    fn after_reap_does_not_close_unrecorded_or_live_native_scopes() {
        let dir = temp_dir();
        let unrecorded = native_log_dir(dir.path(), "unrecorded").unwrap();
        let live = native_log_dir(dir.path(), "live").unwrap();
        mark_native_log_dir_pid(&live, std::process::id()).unwrap();

        assert_eq!(reconcile_native_log_dirs_after_reap(dir.path()).unwrap(), 0);
        assert!(unrecorded.join(NATIVE_ACTIVE_MARKER).exists());
        assert!(live.join(NATIVE_ACTIVE_MARKER).exists());
        assert!(!unrecorded.join(NATIVE_CLOSED_MARKER).exists());
        assert!(!live.join(NATIVE_CLOSED_MARKER).exists());
    }

    #[test]
    fn retention_deletes_old_archives_in_debug_but_not_for_capacity() {
        let dir = temp_dir();
        let old = dir.path().join("lookback.log.rotated-20200101T000000Z-0");
        fs::write(&old, vec![0_u8; 8]).unwrap();
        let active = dir.path().join("lookback.log");
        fs::write(&active, vec![0_u8; 8]).unwrap();

        cleanup_log_dir_with_limits(dir.path(), false, Duration::ZERO, 8).unwrap();

        assert!(
            !old.exists(),
            "debug retention still removes expired archives"
        );
        assert!(active.exists(), "active log must never be removed");
    }

    #[test]
    fn archive_retention_uses_rotation_timestamp_not_source_mtime() {
        let dir = temp_dir();
        let active = dir.path().join("lookback.log");
        let mut writer = RotatingFile::with_max_bytes(active.clone(), 3).unwrap();
        writer.write_all(b"old").unwrap();
        writer.flush().unwrap();
        OpenOptions::new()
            .append(true)
            .open(&active)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        writer.write_all(b"new").unwrap();
        writer.flush().unwrap();

        cleanup_log_dir_with_limits(dir.path(), false, Duration::from_secs(14 * 86400), u64::MAX)
            .unwrap();

        assert_eq!(archive_paths(dir.path()).len(), 1);
    }

    #[test]
    fn debug_retention_keeps_recent_archives_even_when_over_capacity() {
        let dir = temp_dir();
        let archive = archive_path_at(dir.path(), Utc::now() - chrono::Duration::days(1), 0);
        fs::write(&archive, vec![0_u8; 8]).unwrap();
        cleanup_log_dir_with_limits(dir.path(), false, Duration::from_secs(14 * 86400), 1).unwrap();
        assert!(archive.exists());
    }

    #[test]
    fn release_capacity_prunes_oldest_archive_only() {
        let dir = temp_dir();
        let first = archive_path_at(dir.path(), Utc::now() - chrono::Duration::days(2), 0);
        let second = archive_path_at(dir.path(), Utc::now() - chrono::Duration::days(1), 0);
        fs::write(&first, vec![0_u8; 8]).unwrap();
        fs::write(&second, vec![0_u8; 8]).unwrap();
        cleanup_log_dir_with_limits(dir.path(), true, Duration::from_secs(14 * 86400), 8).unwrap();
        assert!(!first.exists());
        assert!(second.exists());
    }

    #[test]
    fn unknown_files_are_not_pruned() {
        let dir = temp_dir();
        let unknown = dir.path().join("notes.txt");
        let unknown_archive = dir.path().join("notes.txt.rotated-20200101T000000Z-0");
        fs::write(&unknown, vec![0_u8; 128]).unwrap();
        fs::write(&unknown_archive, vec![0_u8; 128]).unwrap();
        cleanup_log_dir_with_limits(dir.path(), true, Duration::ZERO, 1).unwrap();
        assert!(unknown.exists());
        assert!(unknown_archive.exists());
    }

    #[test]
    fn unknown_migration_prefixed_files_are_not_pruned() {
        let dir = temp_dir();
        let notes = dir.path().join("database-migration-notes.txt");
        fs::write(&notes, vec![0_u8; 128]).unwrap();

        cleanup_log_dir_with_limits(dir.path(), true, Duration::ZERO, 1).unwrap();

        assert!(notes.exists());
    }

    #[test]
    fn active_migration_log_is_excluded_until_closed() {
        let dir = temp_dir();
        let log = dir.path().join("database-migration-attempt.log");
        fs::write(&log, b"running").unwrap();
        mark_migration_log_active(&log).unwrap();
        cleanup_log_dir_with_limits(dir.path(), true, Duration::ZERO, 0).unwrap();
        assert!(log.exists());

        mark_migration_log_closed(&log).unwrap();
        cleanup_log_dir_with_limits(dir.path(), true, Duration::ZERO, 0).unwrap();
        assert!(!log.exists());
    }

    #[test]
    fn native_archive_directory_is_removed_after_age_in_debug() {
        let dir = temp_dir();
        let native = dir.path().join("native").join("import-closed");
        fs::create_dir_all(&native).unwrap();
        fs::write(native.join("Lookback.log"), b"closed").unwrap();
        mark_native_log_dir_closed(&native).unwrap();

        cleanup_log_dir_with_limits(dir.path(), false, Duration::ZERO, u64::MAX).unwrap();

        assert!(!native.exists(), "expired native logs must be age-pruned");
    }

    #[test]
    fn debug_keeps_recent_native_logs_when_over_capacity() {
        let dir = temp_dir();
        let native = dir.path().join("native").join("import-active");
        fs::create_dir_all(&native).unwrap();
        fs::write(native.join("Lookback.log"), vec![0_u8; 8]).unwrap();

        cleanup_log_dir_with_limits(dir.path(), false, Duration::from_secs(14 * 86400), 1).unwrap();

        assert!(
            native.exists(),
            "debug capacity must not delete native logs"
        );
    }

    #[test]
    fn release_capacity_prunes_oldest_native_directory() {
        let dir = temp_dir();
        let first = dir.path().join("native").join("import-first");
        let second = dir.path().join("native").join("import-second");
        fs::create_dir_all(&first).unwrap();
        fs::write(first.join("Lookback.log"), vec![0_u8; 8]).unwrap();
        mark_native_log_dir_closed(&first).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("Lookback.log"), vec![0_u8; 8]).unwrap();
        mark_native_log_dir_closed(&second).unwrap();

        cleanup_log_dir_with_limits(dir.path(), true, Duration::from_secs(14 * 86400), 8).unwrap();

        assert!(!first.exists(), "oldest native directory should be pruned");
        assert!(second.exists());
    }

    #[test]
    fn stale_active_native_scope_is_reconciled_before_age_cleanup() {
        let dir = temp_dir();
        let native = dir.path().join("native").join("import-999999999-1-0");
        fs::create_dir_all(&native).unwrap();
        fs::write(native.join(NATIVE_ACTIVE_MARKER), b"pid=2000000000\n").unwrap();
        fs::write(native.join("Lookback.log"), b"crashed").unwrap();

        assert_eq!(reconcile_native_log_dirs(dir.path()).unwrap(), 1);
        cleanup_log_dir_with_limits(dir.path(), false, Duration::ZERO, u64::MAX).unwrap();

        assert!(
            !native.exists(),
            "dead native scope should become age-eligible"
        );
    }
}

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use tracing_subscriber::fmt::MakeWriter;

/// Maximum size of an active application log before it is archived.
pub const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;
/// Maximum age of archived logs in both debug and release builds.
pub const LOG_RETENTION: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// Release-only total size limit for managed logs.
pub const LOG_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

const ARCHIVE_MARKER: &str = ".rotated-";
const NATIVE_ACTIVE_MARKER: &str = ".active";
const NATIVE_CLOSED_MARKER: &str = ".closed";
static NATIVE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A file writer that preserves the active path while rotating old content.
///
/// Rotation is performed before a write that would cross the configured
/// limit, or when the UTC day changes. The file handle is dropped before the
/// rename so this remains valid on Windows.
pub struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    bytes: u64,
    day: chrono::NaiveDate,
    max_bytes: u64,
}

impl RotatingFile {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        Self::with_max_bytes(path, LOG_ROTATE_BYTES)
    }

    /// Construct a rotating writer with a caller-specific size limit (for
    /// example, the crash breadcrumb uses a smaller emergency budget).
    pub fn with_max_bytes(path: PathBuf, max_bytes: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let metadata = fs::metadata(&path).ok();
        let bytes = metadata.as_ref().map_or(0, |m| m.len());
        let day = metadata
            .and_then(|m| m.modified().ok())
            .map(|time| DateTime::<Utc>::from(time).date_naive())
            .unwrap_or_else(|| Utc::now().date_naive());
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file: Some(file),
            bytes,
            day,
            max_bytes,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }

        if self.bytes > 0 && self.path.exists() {
            let archive = match archive_path(&self.path) {
                Ok(archive) => archive,
                Err(error) => {
                    self.file = Some(
                        OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&self.path)?,
                    );
                    self.day = Utc::now().date_naive();
                    tracing::debug!(?error, path = ?self.path, "log rotation archive naming failed; continuing active logging");
                    return Ok(());
                }
            };
            if let Err(error) = fs::rename(&self.path, &archive) {
                // Keep logging if another process temporarily prevents the
                // rename (notably a Windows scanner). Reopen the active file
                // and retry on a later write instead of dropping the line.
                self.file = Some(
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&self.path)?,
                );
                self.day = Utc::now().date_naive();
                tracing::debug!(?error, path = ?self.path, "log rotation rename failed; continuing active logging");
                return Ok(());
            }
            // `rename` preserves the old active file's mtime. Refresh the
            // archive timestamp so retention measures rotation age rather
            // than the age of the first byte ever written to the stream.
            if let Ok(file) = OpenOptions::new().append(true).open(&archive) {
                let _ = file.set_modified(SystemTime::now());
            }
        }
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?,
        );
        self.bytes = 0;
        self.day = Utc::now().date_naive();
        Ok(())
    }

    fn rotate_if_needed(&mut self, incoming: usize) -> io::Result<()> {
        let day_changed = self.day != Utc::now().date_naive();
        let over_size =
            self.bytes > 0 && self.bytes.saturating_add(incoming as u64) > self.max_bytes;
        if day_changed || over_size {
            self.rotate()?;
        }
        Ok(())
    }

    /// Flush buffered bytes to the operating-system file handle.
    pub fn sync_all(&mut self) -> io::Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.flush()?;
        file.sync_all()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Best-effort retention maintenance for the writer's directory.
    pub fn cleanup(&self) -> io::Result<CleanupReport> {
        cleanup_log_dir(self.path.parent().unwrap_or_else(|| Path::new(".")))
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_needed(buf.len())?;
        let Some(file) = self.file.as_mut() else {
            return Err(io::Error::other("rotating log file is closed"));
        };
        let written = file.write(buf)?;
        self.bytes = self.bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().map_or(Ok(()), File::flush)
    }
}

/// `tracing_subscriber` writer that shares one rotating file between its
/// per-event writer guards. Locking keeps rotation and writes atomic when
/// multiple tracing spans emit concurrently.
#[derive(Clone)]
pub struct SharedRotatingWriter {
    inner: Arc<Mutex<RotatingFile>>,
}

impl SharedRotatingWriter {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(RotatingFile::new(path)?)),
        })
    }
}

pub struct RotatingWriterGuard {
    inner: Arc<Mutex<RotatingFile>>,
}

impl Write for RotatingWriterGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("rotating log writer lock poisoned"))?
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("rotating log writer lock poisoned"))?
            .flush()
    }
}

impl<'a> MakeWriter<'a> for SharedRotatingWriter {
    type Writer = RotatingWriterGuard;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingWriterGuard {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CleanupReport {
    pub expired: usize,
    pub capacity: usize,
}

/// Apply the profile-specific retention policy. Debug builds prune by age,
/// while only release builds prune by the total managed-log size.
pub fn cleanup_log_dir(log_dir: &Path) -> io::Result<CleanupReport> {
    // Convert crashed child scopes to closed scopes before applying age and
    // release-capacity retention. Live PIDs remain active and are untouched.
    let _ = reconcile_native_log_dirs(log_dir)?;
    cleanup_log_dir_with_limits(
        log_dir,
        !cfg!(debug_assertions),
        LOG_RETENTION,
        LOG_TOTAL_BYTES,
    )
}

/// Create a unique directory for native/auxiliary process logs.
///
/// Native processes are deliberately placed below `log/native/` so their
/// duplicate output cannot be confused with the parent sidecar's canonical
/// stdout/stderr logs. The generated identifier is process-local and safe as
/// a single path component.
pub fn native_log_dir(log_dir: &Path, scope: &str) -> io::Result<PathBuf> {
    let safe_scope = scope
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let millis = Utc::now().timestamp_millis();
    let sequence = NATIVE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let native_root = log_dir.join("native");
    fs::create_dir_all(&native_root)?;
    for attempt in 0_u64..1000 {
        let launch_id = format!("{}-{}-{}-{}", safe_scope, millis, sequence, attempt);
        let path = native_root.join(launch_id);
        match fs::create_dir(&path) {
            Ok(()) => {
                OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(path.join(NATIVE_ACTIVE_MARKER))?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many native log scopes for one timestamp",
    ))
}

/// Record the PID of the native child that owns an active log directory.
///
/// The directory is created before spawning so its path can be passed through
/// the child's environment. Recording the PID after a successful spawn avoids
/// mistaking the parent application's PID for the process that is actually
/// writing the native log.
pub fn mark_native_log_dir_pid(path: &Path, pid: u32) -> io::Result<()> {
    let mut marker = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path.join(NATIVE_ACTIVE_MARKER))?;
    marker.write_all(format!("pid={pid}\n").as_bytes())?;
    marker.sync_all()
}

/// Mark a native child-log directory closed after its process has exited.
/// Cleanup only considers directories carrying this marker, so an active
/// sidecar cannot be removed while it is still writing.
pub fn mark_native_log_dir_closed(path: &Path) -> io::Result<()> {
    let active = path.join(NATIVE_ACTIVE_MARKER);
    let closed = path.join(NATIVE_CLOSED_MARKER);
    if active.exists() {
        fs::remove_file(active)?;
    }
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(closed)?
        .sync_all()
}

fn migration_marker(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("migration.log");
    path.with_file_name(format!("{name}.{suffix}"))
}

/// Mark a migration audit log active while its coordinator is writing.
pub fn mark_migration_log_active(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(migration_marker(path, "closed"));
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(migration_marker(path, "active"))?
        .sync_all()
}

/// Mark a migration audit log closed after its coordinator exits.
pub fn mark_migration_log_closed(path: &Path) -> io::Result<()> {
    let _ = fs::remove_file(migration_marker(path, "active"));
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(migration_marker(path, "closed"))?
        .sync_all()
}

/// Reconcile migration logs left active by a crashed coordinator. Call only
/// after taking the data-root instance lock and before starting a new gate.
pub fn reconcile_migration_logs(log_dir: &Path) -> io::Result<usize> {
    let entries = match fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut reconciled = 0;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if !entry.file_type()?.is_file()
            || !name.starts_with("database-migration-")
            || !name.ends_with(".log")
            || !migration_marker(&path, "active").is_file()
        {
            continue;
        }
        mark_migration_log_closed(&path)?;
        reconciled += 1;
    }
    Ok(reconciled)
}

/// Close stale native scopes left by a crashed child. Normal shutdown marks
/// scopes explicitly; this startup/maintenance reconciliation handles the
/// complementary crash path without ever touching a live process's files.
pub fn reconcile_native_log_dirs(log_dir: &Path) -> io::Result<usize> {
    reconcile_native_log_dirs_impl(log_dir)
}

/// Reconcile active native scopes after the caller has acquired the
/// per-data-root lock and reaped recorded children. A scope is closed only
/// when its marker records a child PID that is no longer alive; unrecorded or
/// live scopes remain active so an orphan cannot be deleted while writing.
pub fn reconcile_native_log_dirs_after_reap(log_dir: &Path) -> io::Result<usize> {
    reconcile_native_log_dirs_impl(log_dir)
}

fn reconcile_native_log_dirs_impl(log_dir: &Path) -> io::Result<usize> {
    let native_root = log_dir.join("native");
    let entries = match fs::read_dir(&native_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut closed = 0;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir()
            || !path.join(NATIVE_ACTIVE_MARKER).is_file()
            || path.join(NATIVE_CLOSED_MARKER).exists()
        {
            continue;
        }
        if native_process_id(&path).is_some_and(|pid| !process_is_alive(pid)) {
            mark_native_log_dir_closed(&path)?;
            closed += 1;
        }
    }
    Ok(closed)
}

fn cleanup_log_dir_with_limits(
    log_dir: &Path,
    capacity_prune: bool,
    retention: Duration,
    total_limit: u64,
) -> io::Result<CleanupReport> {
    let mut report = CleanupReport::default();
    let now = SystemTime::now();
    let cutoff = now.checked_sub(retention).unwrap_or(SystemTime::UNIX_EPOCH);
    let mut candidates = Vec::new();
    let mut active_total = 0_u64;
    let entries = match fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if !file_type.is_file() || !is_managed_name(&path) {
            continue;
        }
        let metadata = entry.metadata()?;
        let active = is_active_name(&path);
        let archive = is_archive_name(&path);
        let migration = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_migration_log_name);
        let migration_active = migration && migration_marker(&path, "active").is_file();
        if migration_active {
            active_total = active_total.saturating_add(metadata.len());
            continue;
        }
        let modified = if migration {
            fs::metadata(migration_marker(&path, "closed"))
                .and_then(|metadata| metadata.modified())
                .or_else(|_| metadata.modified())
                .unwrap_or(now)
        } else {
            archive_timestamp(&path)
                .or_else(|| metadata.modified().ok())
                .unwrap_or(now)
        };
        if !active && (archive || migration) {
            if modified < cutoff && fs::remove_file(&path).is_ok() {
                report.expired += 1;
                continue;
            }
            candidates.push((path, metadata.len(), modified));
        } else if active {
            active_total = active_total.saturating_add(metadata.len());
        }
    }

    collect_native_candidates(
        log_dir,
        cutoff,
        &mut report,
        &mut candidates,
        &mut active_total,
    )?;

    if capacity_prune {
        let mut total =
            active_total.saturating_add(candidates.iter().map(|(_, bytes, _)| *bytes).sum::<u64>());
        // Capacity trimming is intentionally limited to archives and audit
        // migration logs; active files are never deleted by maintenance.
        candidates.sort_by(|left, right| left.2.cmp(&right.2).then_with(|| left.0.cmp(&right.0)));
        for (path, bytes, _) in candidates {
            if total <= total_limit {
                break;
            }
            let removed = if path.is_dir() {
                fs::remove_dir_all(path).is_ok()
            } else {
                fs::remove_file(path).is_ok()
            };
            if removed {
                total = total.saturating_sub(bytes);
                report.capacity += 1;
            }
        }
    }
    Ok(report)
}

/// Collect one level of native child-log directories. A directory is treated
/// as active when any regular file inside it was modified recently; this keeps
/// resident sidecar output safe even though writing a file does not update its
/// parent directory mtime. Unknown files outside this managed namespace are
/// never considered.
fn collect_native_candidates(
    log_dir: &Path,
    cutoff: SystemTime,
    report: &mut CleanupReport,
    candidates: &mut Vec<(PathBuf, u64, SystemTime)>,
    active_total: &mut u64,
) -> io::Result<()> {
    let native_root = log_dir.join("native");
    let entries = match fs::read_dir(&native_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let mut bytes = 0_u64;
        let mut newest = SystemTime::UNIX_EPOCH;
        let mut has_file = false;
        for child in fs::read_dir(&path)? {
            let child = child?;
            if child.file_name() == NATIVE_ACTIVE_MARKER
                || child.file_name() == NATIVE_CLOSED_MARKER
            {
                continue;
            }
            if !child.file_type()?.is_file() {
                continue;
            }
            let metadata = child.metadata()?;
            has_file = true;
            bytes = bytes.saturating_add(metadata.len());
            if let Ok(modified) = metadata.modified()
                && modified > newest
            {
                newest = modified;
            }
        }
        if !has_file {
            continue;
        }
        let active_marker = path.join(NATIVE_ACTIVE_MARKER);
        let closed_marker = path.join(NATIVE_CLOSED_MARKER);
        let active = active_marker.is_file() || !closed_marker.is_file();
        let closed_at = fs::metadata(&closed_marker)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(newest);
        if !active && closed_at < cutoff {
            if fs::remove_dir_all(&path).is_ok() {
                report.expired += 1;
            }
        } else if active {
            // Keep recent native directories out of the deletion candidates;
            // they may belong to a still-running child process.
            *active_total = active_total.saturating_add(bytes);
        } else {
            candidates.push((path, bytes, closed_at));
        }
    }
    Ok(())
}

fn native_process_id(path: &Path) -> Option<u32> {
    let marker = fs::read_to_string(path.join(NATIVE_ACTIVE_MARKER)).ok()?;
    marker
        .trim()
        .strip_prefix("pid=")
        .and_then(|pid| pid.parse::<u32>().ok())
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    // SAFETY: kill(pid, 0) only probes process existence and does not send a
    // signal. The pid came from the directory name and is range-checked.
    unsafe { kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    type Handle = *mut std::ffi::c_void;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit_handle: i32, pid: u32) -> Handle;
        fn GetExitCodeProcess(handle: Handle, code: *mut u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }
    // SAFETY: these calls only query and close the handle for the PID encoded
    // by our own native-log directory name.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0;
        let alive = GetExitCodeProcess(handle, &mut code) != 0 && code == STILL_ACTIVE;
        let _ = CloseHandle(handle);
        alive
    }
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> bool {
    false
}

fn archive_path(path: &Path) -> io::Result<PathBuf> {
    let stamp = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let base = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("log");
    for sequence in 0_u32..1000 {
        let candidate = parent.join(format!("{base}{ARCHIVE_MARKER}{stamp}-{sequence}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many log archives for one timestamp",
    ))
}

fn is_archive_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split_once(ARCHIVE_MARKER))
        .is_some_and(|(base, _)| is_active_basename(base))
}

fn archive_timestamp(path: &Path) -> Option<SystemTime> {
    let name = path.file_name()?.to_str()?;
    let suffix = name.split_once(ARCHIVE_MARKER)?.1;
    let timestamp = suffix.rsplit_once('-')?.0;
    let parsed = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%S%.3fZ")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%SZ"))
        .ok()?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(parsed, Utc).into())
}

fn is_active_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_active_basename)
}

fn is_active_basename(name: &str) -> bool {
    matches!(
        name,
        "lookback.log"
            | "crashtrace.log"
            | "jobworkerp.stdout.log"
            | "jobworkerp.stderr.log"
            | "memories.stdout.log"
            | "memories.stderr.log"
            | "conductor.stdout.log"
            | "conductor.stderr.log"
    )
}

fn is_managed_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name.ends_with(".active") || name.ends_with(".closed") {
        return false;
    }
    is_active_name(path) || is_archive_name(path) || is_migration_log_name(name)
}

fn is_migration_log_name(name: &str) -> bool {
    name.starts_with("database-migration-") && name.ends_with(".log")
}

#[cfg(test)]
fn archive_paths(dir: &Path) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_archive_name(path))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}
