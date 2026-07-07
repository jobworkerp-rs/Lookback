//! Read sidecar logs from inside the app for troubleshooting.
//!
//! The sidecars already append their stdout/stderr to four files under
//! `<data>/log/` (see `sidecar::lifecycle::forward_output`). These can reach
//! tens of MB on a full import (memories span bodies — see the
//! `DEFAULT_SIDECAR_LOG` comment), so we only ever return the *tail*: seek to
//! the end, read the last `max_bytes`, and trim to a line boundary. No live
//! streaming and no rotation in the MVP (rotation is a separate deferred item).

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::AppResult;

use super::AppState;

/// Default tail size when the caller doesn't specify one.
const DEFAULT_MAX_BYTES: usize = 64 * 1024;
/// Hard cap so a hostile / buggy caller can't ask us to buffer the whole file.
const MAX_BYTES_CAP: usize = 1 << 20; // 1 MiB

/// Lookback's own log file name (under `<root>/log/`). Single source of truth:
/// `init_tracing` writes it, the `App` log source reads it — a divergence would
/// leave the UI unable to find the file the app is writing.
pub const APP_LOG_FILE: &str = "lookback.log";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogSource {
    Jobworkerp,
    Memories,
    /// Lookback's own Rust-side log (`<root>/log/lookback.log`). Carries the
    /// `memories-import` child's forwarded stdout/stderr, so it's where a
    /// remote-import failure surfaces. Single file — `LogStream` is ignored.
    App,
}

impl LogSource {
    fn file_stem(self) -> &'static str {
        match self {
            LogSource::Jobworkerp => "jobworkerp",
            LogSource::Memories => "memories",
            // App is a single file (APP_LOG_FILE), not a `<stem>.<stream>.log`
            // pair, so `log_file_name` short-circuits before reading this.
            LogSource::App => "lookback",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

impl LogStream {
    fn file_stem(self) -> &'static str {
        match self {
            LogStream::Stdout => "stdout",
            LogStream::Stderr => "stderr",
        }
    }
}

/// `<source>.<stream>.log`, matching the names `forward_output` writes. The
/// `App` source is a single combined file (`lookback.log`), so the stream
/// selector is ignored for it (init_tracing writes one file, not a stdout /
/// stderr pair like the sidecars).
pub fn log_file_name(source: LogSource, stream: LogStream) -> String {
    match source {
        LogSource::App => APP_LOG_FILE.to_string(),
        _ => format!("{}.{}.log", source.file_stem(), stream.file_stem()),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LogTail {
    pub file_name: String,
    pub content: String,
    /// True when the file was larger than `max_bytes` so only the tail is shown.
    pub truncated: bool,
    pub file_size: u64,
}

/// Decode bytes to a string, lossily, because a tail cut may split a
/// multi-byte char. Line-boundary alignment is `read_tail`'s job (it owns the
/// truncation point), so this is a pure decode with no trimming.
pub fn tail_bytes(data: &[u8]) -> String {
    String::from_utf8_lossy(data).into_owned()
}

/// Align a truncated tail to a clean line. `buf` begins with the single
/// *boundary byte* that precedes the tail window (the byte at `start - 1`),
/// followed by the tail itself. If that boundary byte is `\n`, the tail already
/// starts at a line boundary, so we only drop the boundary byte. Otherwise the
/// tail begins mid-line, so we drop everything up to and including the first
/// newline. With no newline at all (one giant line) we keep the tail (minus the
/// boundary byte) rather than returning nothing.
fn align_tail_after_boundary(buf: &[u8]) -> Vec<u8> {
    // buf[0] is the boundary byte (start-1); the tail is buf[1..].
    if buf.first() == Some(&b'\n') {
        return buf[1..].to_vec();
    }
    match buf.iter().position(|&b| b == b'\n') {
        Some(nl) => buf[nl + 1..].to_vec(),
        None => buf[1..].to_vec(),
    }
}

/// Read the tail bytes of `path` plus its full size and whether it was
/// truncated. When the file exceeds `max_bytes` we seek to the tail and drop a
/// leading *partial* line so the first line returned is never a mid-line
/// fragment — but a tail that already begins exactly at a line boundary keeps
/// its full first line. To tell the two apart we seek one byte earlier (to
/// `start - 1`) and inspect that boundary byte (see `align_tail_after_boundary`).
/// Alignment lives here because this is the layer that owns the truncation point
/// (the previous split between this and `tail_bytes` left the alignment dead,
/// since the caller passed the same `max_bytes` to both). A missing file yields
/// an empty result (size 0) rather than an error so the UI shows "no logs yet"
/// instead of breaking.
pub fn read_tail(path: &Path, max_bytes: usize) -> AppResult<(Vec<u8>, u64, bool)> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), 0, false));
        }
        Err(e) => return Err(e.into()),
    };
    let size = file.metadata()?.len();
    let max = max_bytes as u64;
    let truncated = size > max;
    let start = size.saturating_sub(max);
    // For a truncated read, seek one byte before the window so we can inspect
    // the boundary byte and avoid dropping a first line that's already whole.
    let seek_to = if truncated { start - 1 } else { start };
    if seek_to > 0 {
        file.seek(SeekFrom::Start(seek_to))?;
    }
    let mut buf = Vec::with_capacity(size.min(max + 1) as usize);
    file.read_to_end(&mut buf)?;
    // Only the truncated case has a boundary byte / possible partial leading
    // line; an un-truncated read starts at byte 0 (a real line start).
    if truncated {
        buf = align_tail_after_boundary(&buf);
    }
    Ok((buf, size, truncated))
}

#[tauri::command]
pub async fn read_sidecar_log(
    state: tauri::State<'_, AppState>,
    source: LogSource,
    stream: LogStream,
    max_bytes: Option<usize>,
) -> AppResult<LogTail> {
    let cap = max_bytes.unwrap_or(DEFAULT_MAX_BYTES).min(MAX_BYTES_CAP);
    let file_name = log_file_name(source, stream);
    let path = state.data.log_dir().join(&file_name);
    // Reading and trimming a possibly large file is blocking I/O; keep it off
    // the async runtime's worker threads (mirrors model.rs file scanning).
    let (bytes, file_size, truncated) = tokio::task::spawn_blocking(move || read_tail(&path, cap))
        .await
        .map_err(|e| crate::error::AppError::Config(format!("log read task failed: {e}")))??;
    Ok(LogTail {
        file_name,
        // bytes is already line-aligned by read_tail; this is a pure decode.
        content: tail_bytes(&bytes),
        truncated,
        file_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_bytes_decodes_utf8() {
        assert_eq!(tail_bytes(b"line1\nline2\n"), "line1\nline2\n");
    }

    #[test]
    fn tail_bytes_empty_input_is_empty() {
        assert_eq!(tail_bytes(b""), "");
    }

    #[test]
    fn tail_bytes_multibyte_split_does_not_panic() {
        // A tail cut may land inside a 3-byte char; lossy decode must not panic.
        let s = "あいうえお"; // 15 bytes
        let truncated = &s.as_bytes()[1..]; // starts mid-char
        let out = tail_bytes(truncated);
        // The leading broken char becomes U+FFFD; the rest survives.
        assert!(out.contains("いうえお"));
    }

    #[test]
    fn read_tail_drops_partial_leading_line_when_truncated() {
        // 12-byte file, cap 8 → window starts at byte 4 (mid "line1"); the
        // boundary byte at 3 is 'e' (not \n), so the partial fragment is
        // dropped and the tail starts at the next clean line.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aligned.log");
        std::fs::write(&path, b"line1\nline2\n").unwrap();
        let (bytes, size, truncated) = read_tail(&path, 8).unwrap();
        assert_eq!(size, 12);
        assert!(truncated);
        // Must NOT contain a partial leading "ne1"; should start at line2.
        assert_eq!(tail_bytes(&bytes), "line2\n");
    }

    #[test]
    fn read_tail_keeps_whole_first_line_when_window_starts_at_line_boundary() {
        // 6-byte file "a\nb\nc\n", cap 4 → window starts at byte 2 ('b'), which
        // is already a line start (boundary byte at 1 is \n). The full "b\n"
        // line must survive — not be dropped down to just "c\n".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boundary.log");
        std::fs::write(&path, b"a\nb\nc\n").unwrap();
        let (bytes, size, truncated) = read_tail(&path, 4).unwrap();
        assert_eq!(size, 6);
        assert!(truncated);
        assert_eq!(tail_bytes(&bytes), "b\nc\n");
    }

    #[test]
    fn read_tail_keeps_whole_tail_when_no_newline_in_window() {
        // One giant line with no newline → keep the tail (minus the boundary
        // byte) rather than dropping everything.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oneline.log");
        std::fs::write(&path, b"abcdefghijklmnop").unwrap(); // 16 bytes, no \n
        let (bytes, _size, truncated) = read_tail(&path, 8).unwrap();
        assert!(truncated);
        // window starts at byte 8 ('i'); boundary byte at 7 ('h') is not \n and
        // there is no newline anywhere, so the tail from the window is kept.
        assert_eq!(tail_bytes(&bytes), "ijklmnop");
    }

    #[test]
    fn read_tail_does_not_align_when_not_truncated() {
        // Un-truncated read starts at byte 0 (a real line start); the first
        // line must survive even without a trailing newline.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("whole.log");
        std::fs::write(&path, b"first\nsecond").unwrap();
        let (bytes, _size, truncated) = read_tail(&path, 1024).unwrap();
        assert!(!truncated);
        assert_eq!(tail_bytes(&bytes), "first\nsecond");
    }

    #[test]
    fn file_name_formats_match_forward_output() {
        assert_eq!(
            log_file_name(LogSource::Jobworkerp, LogStream::Stdout),
            "jobworkerp.stdout.log"
        );
        assert_eq!(
            log_file_name(LogSource::Memories, LogStream::Stderr),
            "memories.stderr.log"
        );
    }

    #[test]
    fn app_log_file_name_ignores_stream() {
        // The app log is a single combined file; the stream selector must not
        // change the name (init_tracing writes only lookback.log).
        assert_eq!(
            log_file_name(LogSource::App, LogStream::Stdout),
            "lookback.log"
        );
        assert_eq!(
            log_file_name(LogSource::App, LogStream::Stderr),
            "lookback.log"
        );
    }

    #[test]
    fn read_tail_reports_truncation_for_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let body = "x".repeat(1000);
        std::fs::write(&path, &body).unwrap();
        let (bytes, size, truncated) = read_tail(&path, 100).unwrap();
        assert_eq!(size, 1000);
        assert!(truncated);
        assert_eq!(bytes.len(), 100);
    }

    #[test]
    fn read_tail_no_truncation_for_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.log");
        std::fs::write(&path, b"hello\n").unwrap();
        let (bytes, size, truncated) = read_tail(&path, 1024).unwrap();
        assert_eq!(size, 6);
        assert!(!truncated);
        assert_eq!(bytes, b"hello\n");
    }

    #[test]
    fn read_tail_missing_file_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.log");
        let (bytes, size, truncated) = read_tail(&path, 1024).unwrap();
        assert!(bytes.is_empty());
        assert_eq!(size, 0);
        assert!(!truncated);
    }

    #[test]
    fn enum_serde_uses_snake_case() {
        assert_eq!(
            serde_json::from_str::<LogSource>("\"jobworkerp\"").unwrap(),
            LogSource::Jobworkerp
        );
        assert_eq!(
            serde_json::from_str::<LogSource>("\"memories\"").unwrap(),
            LogSource::Memories
        );
        assert_eq!(
            serde_json::from_str::<LogStream>("\"stdout\"").unwrap(),
            LogStream::Stdout
        );
        assert_eq!(
            serde_json::from_str::<LogStream>("\"stderr\"").unwrap(),
            LogStream::Stderr
        );
    }
}
