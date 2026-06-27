//! Crash-position breadcrumbs that survive a hard OS panic.
//!
//! The External→Local LLM switch can take the whole machine down
//! (`pmap_recycle_page`, see `ai-docs/external-to-local-llm-crash-
//! investigation.md`). When the kernel panics, tracing's buffered file
//! appender loses whatever it hadn't flushed, so the normal logs stop short of
//! the real crash site and we can't tell which step was executing.
//!
//! [`mark`] writes one line to `<data>/log/crashtrace.log` and immediately
//! `fsync`s it, so the LAST line in that file is the last step that STARTED
//! before the machine died — a reliable "we got at least this far" breadcrumb.
//! It is intentionally heavyweight (open + write + fsync per call); only sprinkle
//! it at coarse phase boundaries, not in hot loops.
//!
//! This is a diagnostic facility for isolating the crash position; remove the
//! call sites once the root cause is fixed.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Absolute path to the crashtrace file. Set once at startup via [`init`];
/// before that (or if init was skipped) `mark` falls back to stderr only.
static PATH: OnceLock<PathBuf> = OnceLock::new();

/// Point the crashtrace at `<log_dir>/crashtrace.log`. Best-effort: a second
/// call is ignored. Safe to call before the dir exists — `mark` creates it.
pub fn init(log_dir: PathBuf) {
    let _ = PATH.set(log_dir.join("crashtrace.log"));
}

/// Append `msg` (with a monotonic-ish wall-clock prefix) to the crashtrace file
/// and fsync. Also echoes to stderr so `pnpm tauri:dev` shows it inline. Never
/// panics — every IO error is swallowed (a breadcrumb that fails to write must
/// not itself crash the app).
pub fn mark(msg: &str) {
    // Stderr first: even if the file write races the panic, the dev console may
    // still capture the line.
    eprintln!("[crashtrace] {msg}");
    let Some(path) = PATH.get() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        // No timestamp crate dependency here: a coarse SystemTime is enough to
        // correlate with the OS panic time.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let _ = writeln!(f, "{now:.3} {msg}");
        // The whole point: get the bytes onto disk before the kernel dies.
        let _ = f.flush();
        let _ = f.sync_all();
    }
}
