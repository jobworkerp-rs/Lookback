//! Crash-safe sidecar reaping.
//!
//! macOS lacks Linux's `PR_SET_PDEATHSIG`, so a `kill -9` (or panic) of the
//! Tauri host never runs `Sidecars::drop` / `kill_on_drop`, stranding the
//! `all-in-one` / memories `front` children. The orphans keep listening on
//! 9000/9010, which pushes the next launch onto random fallback ports and
//! mixes up the gRPC targets.
//!
//! To close the gap we record each spawned sidecar's `(pid, exe-path)` in a
//! file under the data root at startup and reap any survivors on the *next*
//! launch — before port selection. The exe-path is matched against the live
//! process so a recycled PID belonging to an unrelated program is never
//! killed.
//!
//! A second *concurrent* app instance must NOT reap the first one's still-live
//! children. The exe-path match alone can't tell a crash orphan from another
//! instance's running sidecar (same binary). So reaping is gated on an advisory
//! `flock` over `sidecar.lock`: each running instance holds it for its whole
//! lifetime, and a new launch reaps only if it can acquire the lock
//! non-blocking. The kernel releases the lock when the holder exits — including
//! on `kill -9` — so the lock is held exactly while an instance is alive, which
//! is precisely the distinction we need.

use std::path::Path;

use tracing::{info, warn};

/// Outcome of `reap_recorded`: whether reaping ran and the held instance lock.
/// The caller MUST keep `lock` alive for the app's lifetime — dropping it frees
/// the advisory lock, which would let another launch treat this instance's live
/// sidecars as orphans.
pub struct ReapOutcome {
    pub reaped: usize,
    /// `Some` when this launch owns the instance lock (normal case). `None`
    /// when another live instance holds it — reaping was skipped to avoid
    /// killing that instance's sidecars.
    pub lock: Option<InstanceLock>,
}

/// One recorded sidecar process: its PID plus the absolute path of the
/// binary it was spawned from. The path guards against PID reuse — by the
/// next launch the OS may have handed the number to an unrelated process, so
/// we only reap when the live process's executable still matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidEntry {
    pub pid: u32,
    pub exe: String,
}

/// Serialize entries to the `sidecar.pids` file body: one `pid<TAB>exe` line
/// each. A trailing newline keeps the file POSIX-friendly.
pub fn serialize(entries: &[PidEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        out.push_str(&e.pid.to_string());
        out.push('\t');
        out.push_str(&e.exe);
        out.push('\n');
    }
    out
}

/// Parse the `sidecar.pids` body. Malformed / blank lines are skipped rather
/// than failing the whole reap — a corrupt file must never block startup.
pub fn parse(body: &str) -> Vec<PidEntry> {
    body.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (pid_str, exe) = line.split_once('\t')?;
            let pid = pid_str.trim().parse::<u32>().ok()?;
            let exe = exe.trim();
            if exe.is_empty() {
                return None;
            }
            Some(PidEntry {
                pid,
                exe: exe.to_string(),
            })
        })
        .collect()
}

/// Write the recorded PIDs, best-effort. A failure here only means the next
/// launch can't reap — it must not abort the current spawn, so the caller
/// logs and continues.
pub fn write_pids(path: &Path, entries: &[PidEntry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serialize(entries))
}

/// Remove the PID file (graceful-stop path). Missing file is success.
pub fn clear_pids(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(?path, error = ?e, "failed to clear sidecar pid file"),
    }
}

/// Acquire the instance lock, then reap any survivors recorded at `pids_path`.
///
/// The lock at `lock_path` gates the reap: if another live instance already
/// holds it, [`acquire_instance_lock`] returns `None` and we reap nothing — the
/// recorded PIDs are that instance's live children, not crash orphans. On a
/// clean launch we own the lock; the returned [`ReapOutcome::lock`] MUST be kept
/// alive for the whole session so a concurrent launch sees the lock held.
///
/// When we own the lock, each recorded entry is reaped only when the live
/// process's executable path still matches the recorded one (guarding against
/// PID reuse), then the PID file is cleared.
pub fn reap_recorded(pids_path: &Path, lock_path: &Path) -> ReapOutcome {
    let Some(lock) = acquire_instance_lock(lock_path) else {
        // Another live instance owns the lock: its recorded sidecars are
        // running, not orphaned. Leave both the PID file and processes intact.
        warn!(
            ?lock_path,
            "another app instance holds the sidecar lock; skipping orphan reap",
        );
        return ReapOutcome {
            reaped: 0,
            lock: None,
        };
    };

    let reaped = match std::fs::read_to_string(pids_path) {
        Ok(body) => {
            let mut n = 0;
            for entry in parse(&body) {
                match process_exe(entry.pid) {
                    Some(exe) if exe == entry.exe => {
                        info!(pid = entry.pid, exe = %entry.exe, "reaping orphaned sidecar");
                        kill_hard(entry.pid);
                        n += 1;
                    }
                    Some(exe) => {
                        // PID was recycled by an unrelated process — never kill it.
                        warn!(
                            pid = entry.pid,
                            recorded = %entry.exe,
                            live = %exe,
                            "recorded sidecar pid now belongs to a different process; skipping",
                        );
                    }
                    None => {
                        // Already gone (graceful exit we didn't observe, or
                        // reaped earlier). Nothing to do.
                    }
                }
            }
            n
        }
        // No PID file (first launch, or graceful prior stop). Nothing to reap;
        // we still return the lock so this instance owns it going forward.
        Err(_) => 0,
    };
    clear_pids(pids_path);
    ReapOutcome {
        reaped,
        lock: Some(lock),
    }
}

/// Public, testable wrapper over [`process_exe`] used at record time so the
/// stored exe string is byte-identical to what reaping will later read back
/// from `ps` — comparing the raw `SidecarConfig` binary path (which may be
/// relative or contain `..`) against `ps`'s normalized output would never
/// match, defeating the recycled-PID guard.
pub fn live_exe(pid: u32) -> Option<String> {
    process_exe(pid)
}

/// Handle to the held instance lock. Keep it alive for the session: dropping
/// it closes the file, which releases the advisory `flock` as a side effect —
/// that would let a concurrent launch treat this instance's sidecars as
/// orphans. The owned `File` does the close on drop; we only retain it.
#[cfg(all(unix, not(test)))]
pub struct InstanceLock {
    _file: std::fs::File,
}

/// Try to take the per-instance advisory lock at `path`, non-blocking. Returns
/// `Some` when this process now holds it, `None` when another live process does
/// (or on any I/O error — failing closed means we skip reaping rather than risk
/// killing a live instance's children).
#[cfg(all(unix, not(test)))]
pub fn acquire_instance_lock(path: &Path) -> Option<InstanceLock> {
    use std::os::unix::io::AsRawFd;

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // The `File` owns the fd; its drop closes it (and releases the flock). We
    // keep it inside `InstanceLock` so the lock lives as long as this instance.
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => {
            warn!(?path, error = ?e, "failed to open sidecar lock file; skipping reap");
            return None;
        }
    };

    // LOCK_EX | LOCK_NB: exclusive, non-blocking. EWOULDBLOCK => held elsewhere,
    // so we drop the file (releasing nothing we held) and report "not ours".
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    // SAFETY: `flock` is safe to call on a valid fd owned by `file`, which
    // outlives the call. A non-zero return (EWOULDBLOCK / error) just means we
    // didn't get the lock.
    let rc = unsafe { libc_flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if rc == 0 {
        Some(InstanceLock { _file: file })
    } else {
        None
    }
}

#[cfg(all(unix, not(test)))]
unsafe fn libc_flock(fd: i32, operation: i32) -> i32 {
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    unsafe { flock(fd, operation) }
}

/// Non-unix fallback: no advisory lock available, so we treat every launch as
/// the sole owner. The orphan-vs-live ambiguity this guards against is the
/// macOS crash case; other platforms keep the prior best-effort behavior.
#[cfg(all(not(unix), not(test)))]
pub struct InstanceLock;

#[cfg(all(not(unix), not(test)))]
pub fn acquire_instance_lock(_path: &Path) -> Option<InstanceLock> {
    Some(InstanceLock)
}

/// Resolve the executable path of a live process, or `None` if it isn't
/// running. Uses `ps -o comm=` which prints the full executable path on macOS
/// for a still-running pid and nothing for a dead one.
#[cfg(not(test))]
fn process_exe(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let exe = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if exe.is_empty() { None } else { Some(exe) }
}

/// SIGKILL the pid directly. We skip the SIGTERM grace window used for our own
/// children (`stop_child`): a reaped orphan is from a previous launch we no
/// longer track, so there is no graceful-shutdown handshake to wait on.
#[cfg(all(unix, not(test)))]
fn kill_hard(pid: u32) {
    // SAFETY: `kill(2)` is async-signal-safe; an out-of-range / dead pid just
    // returns ESRCH which we ignore.
    unsafe {
        let _ = libc_kill(pid as i32, 9 /* SIGKILL */);
    }
}

#[cfg(all(not(unix), not(test)))]
fn kill_hard(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output();
}

#[cfg(all(unix, not(test)))]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, sig) }
}

// ---- test seams -----------------------------------------------------------
//
// `reap_recorded` is the orchestration we want covered, but it must not shell
// out to `ps` or actually SIGKILL anything under `cargo test`. The two
// eff's are swapped for table-driven fakes keyed off the data files the test
// writes, so the parse → match → kill → clear flow is exercised end-to-end.

#[cfg(test)]
thread_local! {
    static LIVE_PROCS: std::cell::RefCell<std::collections::HashMap<u32, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    static KILLED: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Lock paths currently held in-process, modelling `flock`'s exclusivity.
    /// `acquire_instance_lock` refuses a path already present; the test
    /// `InstanceLock`'s Drop removes it, mirroring fd-close releasing the lock.
    static HELD_LOCKS: std::cell::RefCell<std::collections::HashSet<std::path::PathBuf>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

#[cfg(test)]
fn process_exe(pid: u32) -> Option<String> {
    LIVE_PROCS.with(|m| m.borrow().get(&pid).cloned())
}

#[cfg(test)]
fn kill_hard(pid: u32) {
    KILLED.with(|k| k.borrow_mut().push(pid));
}

/// Test double for the instance lock. Holds the path so Drop can release it,
/// emulating the kernel freeing the advisory lock when the holder's fd closes.
#[cfg(test)]
pub struct InstanceLock {
    path: std::path::PathBuf,
}

#[cfg(test)]
impl Drop for InstanceLock {
    fn drop(&mut self) {
        HELD_LOCKS.with(|h| {
            h.borrow_mut().remove(&self.path);
        });
    }
}

#[cfg(test)]
pub fn acquire_instance_lock(path: &Path) -> Option<InstanceLock> {
    HELD_LOCKS.with(|h| {
        let mut held = h.borrow_mut();
        if held.contains(path) {
            None
        } else {
            held.insert(path.to_path_buf());
            Some(InstanceLock {
                path: path.to_path_buf(),
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_live(procs: &[(u32, &str)]) {
        LIVE_PROCS.with(|m| {
            let mut m = m.borrow_mut();
            m.clear();
            for (pid, exe) in procs {
                m.insert(*pid, (*exe).to_string());
            }
        });
        KILLED.with(|k| k.borrow_mut().clear());
    }

    fn killed() -> Vec<u32> {
        KILLED.with(|k| k.borrow().clone())
    }

    fn reset_locks() {
        HELD_LOCKS.with(|h| h.borrow_mut().clear());
    }

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lookback-reaper-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tmp_file(name: &str) -> std::path::PathBuf {
        tmp_dir(name).join("sidecar.pids")
    }

    fn lock_file(name: &str) -> std::path::PathBuf {
        tmp_dir(name).join("sidecar.lock")
    }

    #[test]
    fn serialize_parse_roundtrip() {
        let entries = vec![
            PidEntry {
                pid: 100,
                exe: "/path/all-in-one".into(),
            },
            PidEntry {
                pid: 200,
                exe: "/path/front".into(),
            },
        ];
        let body = serialize(&entries);
        assert_eq!(parse(&body), entries);
    }

    #[test]
    fn parse_skips_malformed_lines() {
        let body = "100\t/a\nnot-a-pid\t/b\n300\n\n400\t/d\n";
        // "not-a-pid" (bad pid), "300" (no tab/exe), blank → skipped.
        assert_eq!(
            parse(body),
            vec![
                PidEntry {
                    pid: 100,
                    exe: "/a".into()
                },
                PidEntry {
                    pid: 400,
                    exe: "/d".into()
                },
            ]
        );
    }

    #[test]
    fn reap_kills_only_matching_live_exe() {
        reset_locks();
        let path = tmp_file("match");
        let lock = lock_file("match");
        write_pids(
            &path,
            &[
                PidEntry {
                    pid: 100,
                    exe: "/bin/all-in-one".into(),
                },
                PidEntry {
                    pid: 200,
                    exe: "/bin/front".into(),
                },
            ],
        )
        .unwrap();
        // Both still alive with the recorded exe.
        set_live(&[(100, "/bin/all-in-one"), (200, "/bin/front")]);

        let outcome = reap_recorded(&path, &lock);
        assert_eq!(outcome.reaped, 2);
        assert!(outcome.lock.is_some(), "owns the lock on a clean launch");
        let mut k = killed();
        k.sort_unstable();
        assert_eq!(k, vec![100, 200]);
        // File is cleared after reaping.
        assert!(!path.exists());
    }

    #[test]
    fn reap_skips_dead_pid() {
        reset_locks();
        let path = tmp_file("dead");
        let lock = lock_file("dead");
        write_pids(
            &path,
            &[PidEntry {
                pid: 100,
                exe: "/bin/all-in-one".into(),
            }],
        )
        .unwrap();
        // Nothing live → already gone.
        set_live(&[]);

        let outcome = reap_recorded(&path, &lock);
        assert_eq!(outcome.reaped, 0);
        assert!(outcome.lock.is_some());
        assert!(killed().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn reap_skips_recycled_pid() {
        reset_locks();
        let path = tmp_file("recycled");
        let lock = lock_file("recycled");
        write_pids(
            &path,
            &[PidEntry {
                pid: 100,
                exe: "/bin/all-in-one".into(),
            }],
        )
        .unwrap();
        // PID 100 now belongs to an unrelated program — must NOT be killed.
        set_live(&[(100, "/usr/bin/Safari")]);

        let outcome = reap_recorded(&path, &lock);
        assert_eq!(outcome.reaped, 0);
        assert!(outcome.lock.is_some());
        assert!(killed().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn reap_missing_file_is_noop() {
        reset_locks();
        let path = tmp_file("missing");
        let lock = lock_file("missing");
        clear_pids(&path); // ensure absent
        let outcome = reap_recorded(&path, &lock);
        assert_eq!(outcome.reaped, 0);
        // Even with no PID file, the launch must own the lock going forward.
        assert!(outcome.lock.is_some());
    }

    #[test]
    fn reap_skipped_when_another_instance_holds_lock() {
        reset_locks();
        let path = tmp_file("held");
        let lock = lock_file("held");
        write_pids(
            &path,
            &[PidEntry {
                pid: 100,
                exe: "/bin/all-in-one".into(),
            }],
        )
        .unwrap();
        // The recorded sidecar is alive AND matches the exe — but it's another
        // running instance's child, not an orphan.
        set_live(&[(100, "/bin/all-in-one")]);

        // Simulate a first, still-running instance owning the lock.
        let first = acquire_instance_lock(&lock).expect("first instance takes the lock");

        let outcome = reap_recorded(&path, &lock);
        assert_eq!(outcome.reaped, 0, "must not reap a live instance's sidecar");
        assert!(
            outcome.lock.is_none(),
            "second launch does not own the lock"
        );
        assert!(killed().is_empty(), "the live sidecar is never killed");
        // The PID file is left intact so the owning instance can still reap on
        // its own next launch.
        assert!(path.exists());

        // When the first instance exits (lock released), a fresh launch reaps.
        drop(first);
        let outcome2 = reap_recorded(&path, &lock);
        assert_eq!(outcome2.reaped, 1);
        assert!(outcome2.lock.is_some());
        assert_eq!(killed(), vec![100]);
        assert!(!path.exists());
    }

    #[test]
    fn instance_lock_releases_on_drop() {
        reset_locks();
        let lock = lock_file("release");
        let held = acquire_instance_lock(&lock).expect("acquire once");
        assert!(
            acquire_instance_lock(&lock).is_none(),
            "second acquire blocked while held"
        );
        drop(held);
        assert!(
            acquire_instance_lock(&lock).is_some(),
            "acquire succeeds after the holder drops"
        );
    }
}
