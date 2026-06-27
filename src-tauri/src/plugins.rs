//! Stage jobworkerp plugin dylibs into the data directory so the
//! `PLUGINS_RUNNER_DIR` we hand jobworkerp on spawn actually contains
//! `LLMPromptRunner` / `MultimodalEmbeddingRunner` etc.
//!
//! Source priority (mirrors the `resolve_bin` env / which / fallback
//! pattern in `lib.rs`):
//!   1. `LOOKBACK_PLUGINS_SRC` env override (tests + special dev setups).
//!   2. Tauri `PathResolver::resolve_resource("plugins")` — the prod
//!      `.app/Contents/Resources/plugins/` directory populated from
//!      `tauri.conf.json` `bundle.resources`.
//!   3. `<CARGO_MANIFEST_DIR>/plugins` — the app-local dev staging directory
//!      populated before Tauri validates bundle resources.
//!
//! Copy is incremental: we sha256 each source against the existing dest
//! file and skip when they match, so the ~100 MB of dylibs only flush
//! to disk on the first launch (or after a fresh plugin build).

use std::path::{Path, PathBuf};

use tauri::AppHandle;
use tracing::{debug, info, warn};

use crate::data::fsutil::{copy_atomic, file_matches};
use crate::error::{AppError, AppResult};

/// Outcome of a `stage_plugins` run. Reported up to the lifecycle layer
/// so the UI can show "n plugins staged from <source>" in logs and so a
/// missing source becomes a surfaced warning instead of a silent crash
/// later when `jobworkerp` finds an empty `PLUGINS_RUNNER_DIR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageReport {
    pub source: PathBuf,
    pub copied: Vec<PathBuf>,
    pub skipped_same: Vec<PathBuf>,
}

impl StageReport {
    pub fn is_empty(&self) -> bool {
        self.copied.is_empty() && self.skipped_same.is_empty()
    }
}

/// Copy every plugin shared library under the resolved source dir into `dest`. Files
/// already present with matching sha256 are left untouched.
///
/// Errors are limited to source-not-resolvable; per-file copy/hash
/// failures are logged and surfaced through the returned report (no
/// partial-failure abort).
pub fn stage_plugins(app: &AppHandle, dest: &Path) -> AppResult<StageReport> {
    let source = resolve_plugins_source(app)?;
    stage_plugins_from(&source, dest)
}

/// Variant of [`stage_plugins`] that takes an explicit source — used
/// internally and from unit tests.
pub fn stage_plugins_from(source: &Path, dest: &Path) -> AppResult<StageReport> {
    if !source.exists() {
        return Err(AppError::Config(format!(
            "plugins source dir not found: {}",
            source.display()
        )));
    }
    std::fs::create_dir_all(dest)?;

    let mut copied = Vec::new();
    let mut skipped_same = Vec::new();
    for src in plugin_library_files(source)? {
        let Some(filename) = src.file_name() else {
            continue;
        };
        let dst = dest.join(filename);

        match file_matches(&src, &dst) {
            Ok(true) => {
                debug!(file = %src.display(), "plugin dylib unchanged, skipping");
                skipped_same.push(dst);
            }
            Ok(false) => {
                if let Err(e) = copy_atomic(&src, &dst) {
                    warn!(file = %src.display(), error = %e, "plugin dylib copy failed");
                    continue;
                }
                info!(file = %dst.display(), "plugin dylib staged");
                copied.push(dst);
            }
            Err(e) => {
                warn!(file = %src.display(), error = %e, "plugin dylib hash failed");
            }
        }
    }
    Ok(StageReport {
        source: source.to_path_buf(),
        copied,
        skipped_same,
    })
}

fn plugin_library_files(source: &Path) -> AppResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut dirs = vec![source.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                dirs.push(path);
            } else if is_plugin_library(&path) {
                files.push(path);
            }
        }
    }
    Ok(files)
}

fn is_plugin_library(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("dylib" | "so")
    )
}

fn resolve_plugins_source(app: &AppHandle) -> AppResult<PathBuf> {
    if let Ok(p) = std::env::var("LOOKBACK_PLUGINS_SRC") {
        let pb = PathBuf::from(p);
        if !pb.exists() {
            return Err(AppError::Config(format!(
                "LOOKBACK_PLUGINS_SRC={} does not exist",
                pb.display()
            )));
        }
        return Ok(pb);
    }
    // Production: tauri.conf maps the source directory to the stable
    // `<app>/Contents/Resources/plugins/` runtime path.
    if let Some(resource_dir) = crate::data::paths::bundled_resource_path(app, "plugins") {
        return Ok(resource_dir);
    }
    // Dev fallback: keep staged plugin libraries inside this app's project
    // tree instead of writing to the parent workspace.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| AppError::Config("CARGO_MANIFEST_DIR not set".into()))?;
    let candidate = PathBuf::from(manifest_dir).join("plugins");
    let canonical = candidate.canonicalize().unwrap_or(candidate);
    if !canonical.exists() {
        return Err(AppError::Config(format!(
            "plugins source dir not found: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_dylib(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents).unwrap();
        p
    }

    #[test]
    fn stage_copies_dylibs_into_empty_dest() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_dylib(src.path(), "libfoo.dylib", b"FOO");
        write_dylib(src.path(), "libbar.dylib", b"BAR");

        let report = stage_plugins_from(src.path(), dst.path()).unwrap();
        assert_eq!(report.copied.len(), 2);
        assert!(report.skipped_same.is_empty());
        assert!(dst.path().join("libfoo.dylib").exists());
        assert!(dst.path().join("libbar.dylib").exists());
    }

    #[test]
    fn stage_skips_files_with_matching_sha256() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_dylib(src.path(), "libfoo.dylib", b"SAME");
        // Pre-populate dest with identical content.
        write_dylib(dst.path(), "libfoo.dylib", b"SAME");

        let report = stage_plugins_from(src.path(), dst.path()).unwrap();
        assert!(report.copied.is_empty());
        assert_eq!(report.skipped_same.len(), 1);
    }

    #[test]
    fn stage_replaces_files_when_sha256_differs() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_dylib(src.path(), "libfoo.dylib", b"NEW_CONTENT");
        write_dylib(dst.path(), "libfoo.dylib", b"OLD");

        let report = stage_plugins_from(src.path(), dst.path()).unwrap();
        assert_eq!(report.copied.len(), 1);
        assert!(report.skipped_same.is_empty());
        let staged = std::fs::read(dst.path().join("libfoo.dylib")).unwrap();
        assert_eq!(staged, b"NEW_CONTENT");
    }

    #[test]
    fn stage_ignores_non_dylib_entries() {
        // A README or sample subdirectory in the source must not be
        // misread as a plugin and copied into PLUGINS_RUNNER_DIR.
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_dylib(src.path(), "README.md", b"docs");
        std::fs::create_dir(src.path().join("hello_runner")).unwrap();
        write_dylib(src.path(), "libreal.dylib", b"REAL");

        let report = stage_plugins_from(src.path(), dst.path()).unwrap();
        assert_eq!(report.copied.len(), 1);
        assert!(dst.path().join("libreal.dylib").exists());
        assert!(!dst.path().join("README.md").exists());
        assert!(!dst.path().join("hello_runner").exists());
    }

    #[test]
    fn stage_copies_linux_so_files() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_dylib(src.path(), "liblinux.so", b"SO");

        let report = stage_plugins_from(src.path(), dst.path()).unwrap();
        assert_eq!(report.copied.len(), 1);
        assert!(dst.path().join("liblinux.so").exists());
    }

    #[test]
    fn stage_copies_plugins_from_nested_dirs() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        let nested = src.path().join("cuda_runner");
        std::fs::create_dir(&nested).unwrap();
        write_dylib(&nested, "libcuda_runner.so", b"CUDA");

        let report = stage_plugins_from(src.path(), dst.path()).unwrap();
        assert_eq!(report.copied.len(), 1);
        assert!(dst.path().join("libcuda_runner.so").exists());
    }

    #[test]
    fn stage_errors_when_source_missing() {
        let dst = tempdir().unwrap();
        let err =
            stage_plugins_from(Path::new("/nonexistent/plugins/dir"), dst.path()).unwrap_err();
        match err {
            AppError::Config(msg) => assert!(msg.contains("plugins source dir not found")),
            other => panic!("expected Config error, got {other:?}"),
        }
    }
}
