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

use std::{
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tracing::{debug, info, warn};

use crate::data::fsutil::{copy_atomic, file_matches, sha256};
use crate::error::{AppError, AppResult};

const MANIFEST_FILE_NAME: &str = ".lookback-plugin-manifest.json";
static MANIFEST_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Ownership record for plugin libraries copied by Lookback. This is kept
/// separate from the libraries themselves so user-provided plugins stay out
/// of cleanup decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PluginManifest {
    libraries: Vec<PluginManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PluginManifestEntry {
    name: String,
    sha256: String,
}

/// Outcome of a `stage_plugins` run. Reported up to the lifecycle layer
/// so the UI can show "n plugins staged from <source>" in logs and so a
/// missing source becomes a surfaced warning instead of a silent crash
/// later when `jobworkerp` finds an empty `PLUGINS_RUNNER_DIR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageReport {
    pub source: PathBuf,
    pub copied: Vec<PathBuf>,
    pub skipped_same: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
}

impl StageReport {
    pub fn is_empty(&self) -> bool {
        self.copied.is_empty() && self.skipped_same.is_empty() && self.deleted.is_empty()
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

    let previous_manifest = read_manifest(dest);
    let source_files = plugin_library_files(source)?;
    let source_names: HashSet<_> = source_files
        .iter()
        .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
        .map(str::to_owned)
        .collect();
    let mut copied = Vec::new();
    let mut skipped_same = Vec::new();
    let mut manifest_entries = Vec::new();
    let mut staging_failed = false;
    for src in source_files {
        let Some(filename) = src.file_name() else {
            continue;
        };
        let dst = dest.join(filename);

        let staged = match file_matches(&src, &dst) {
            Ok(true) => {
                debug!(file = %src.display(), "plugin dylib unchanged, skipping");
                skipped_same.push(dst.clone());
                true
            }
            Ok(false) => {
                if let Err(e) = copy_atomic(&src, &dst) {
                    warn!(file = %src.display(), error = %e, "plugin dylib copy failed");
                    staging_failed = true;
                    false
                } else {
                    info!(file = %dst.display(), "plugin dylib staged");
                    copied.push(dst.clone());
                    true
                }
            }
            Err(e) => {
                warn!(file = %src.display(), error = %e, "plugin dylib hash failed");
                staging_failed = true;
                false
            }
        };

        match (staged, sha256(&src)) {
            (false, _) => {}
            (true, Ok(digest)) => {
                let entry = PluginManifestEntry {
                    name: filename.to_string_lossy().into_owned(),
                    sha256: hex_digest(&digest),
                };
                if copied.iter().any(|path| path == &dst)
                    || previous_manifest
                        .as_ref()
                        .and_then(|manifest| manifest.libraries.iter().find(|old| *old == &entry))
                        .is_some()
                {
                    manifest_entries.push(entry);
                }
            }
            (true, Err(e)) => {
                warn!(file = %src.display(), error = %e, "plugin dylib manifest digest failed");
                staging_failed = true;
            }
        }
    }
    manifest_entries.sort_by(|left, right| left.name.cmp(&right.name));

    let deleted = if staging_failed {
        warn!(
            "plugin staging was incomplete; preserving the previous ownership manifest and stale libraries"
        );
        Vec::new()
    } else {
        let stale_entries = previous_manifest
            .as_ref()
            .map(|manifest| stale_entries(manifest, &source_names))
            .unwrap_or_default();
        // Commit a manifest that still owns every stale library before
        // deleting it. A failed write therefore cannot delete a file without
        // a durable retry record.
        write_manifest_atomic(
            dest,
            &PluginManifest {
                libraries: combined_entries(&manifest_entries, &stale_entries),
            },
        )?;
        let cleanup = remove_stale_libraries(&stale_entries, dest);
        // Keep only removal failures in the final ownership record. A future
        // staging pass can then retry them without adopting user replacements.
        write_manifest_atomic(
            dest,
            &PluginManifest {
                libraries: combined_entries(&manifest_entries, &cleanup.retry_entries),
            },
        )?;
        cleanup.deleted
    };

    Ok(StageReport {
        source: source.to_path_buf(),
        copied,
        skipped_same,
        deleted,
    })
}

/// Missing or malformed manifests deliberately disable cleanup: without a
/// trustworthy ownership list, a dylib in the runtime directory may belong to
/// the user rather than Lookback.
fn read_manifest(dest: &Path) -> Option<PluginManifest> {
    let path = dest.join(MANIFEST_FILE_NAME);
    let contents = match std::fs::read(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(file = %path.display(), error = %e, "plugin manifest could not be read; stale cleanup skipped");
            return None;
        }
    };
    match serde_json::from_slice(&contents) {
        Ok(manifest) => Some(manifest),
        Err(e) => {
            warn!(file = %path.display(), error = %e, "plugin manifest is invalid; stale cleanup skipped");
            None
        }
    }
}

fn stale_entries(
    manifest: &PluginManifest,
    source_names: &HashSet<String>,
) -> Vec<PluginManifestEntry> {
    manifest
        .libraries
        .iter()
        .filter(|entry| !source_names.contains(&entry.name) && is_safe_library_name(&entry.name))
        .cloned()
        .collect()
}

fn combined_entries(
    source_entries: &[PluginManifestEntry],
    retry_entries: &[PluginManifestEntry],
) -> Vec<PluginManifestEntry> {
    let mut entries = source_entries.to_vec();
    entries.extend_from_slice(retry_entries);
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries.dedup_by(|left, right| left.name == right.name);
    entries
}

struct StaleCleanup {
    deleted: Vec<PathBuf>,
    retry_entries: Vec<PluginManifestEntry>,
}

fn remove_stale_libraries(entries: &[PluginManifestEntry], dest: &Path) -> StaleCleanup {
    let mut deleted = Vec::new();
    let mut retry_entries = Vec::new();
    for entry in entries {
        let path = dest.join(&entry.name);
        match sha256(&path) {
            Ok(digest) if hex_digest(&digest) == entry.sha256 => {}
            Ok(_) => {
                warn!(file = %path.display(), "stale plugin name has a different digest; preserving it");
                continue;
            }
            Err(AppError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warn!(file = %path.display(), error = %e, "stale plugin digest could not be verified; preserving it");
                retry_entries.push(entry.clone());
                continue;
            }
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                info!(file = %path.display(), "stale Lookback plugin dylib removed");
                deleted.push(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(file = %path.display(), error = %e, "stale Lookback plugin dylib removal failed");
                retry_entries.push(entry.clone());
            }
        }
    }
    StaleCleanup {
        deleted,
        retry_entries,
    }
}

fn is_safe_library_name(name: &str) -> bool {
    Path::new(name).file_name().and_then(|file| file.to_str()) == Some(name)
        && is_plugin_library(Path::new(name))
}

fn write_manifest_atomic(dest: &Path, manifest: &PluginManifest) -> AppResult<()> {
    let path = dest.join(MANIFEST_FILE_NAME);
    let partial = dest.join(format!(
        "{MANIFEST_FILE_NAME}.{}.{}.partial",
        std::process::id(),
        MANIFEST_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let bytes = serde_json::to_vec(manifest).map_err(anyhow::Error::from)?;
    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&partial)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&partial, &path)
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&partial);
        return Err(e.into());
    }
    Ok(())
}

fn hex_digest(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
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
    fn stage_removes_only_previously_managed_libraries_absent_from_source() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_dylib(src.path(), "libold.dylib", b"OLD");
        stage_plugins_from(src.path(), dst.path()).unwrap();

        std::fs::remove_file(src.path().join("libold.dylib")).unwrap();
        write_dylib(src.path(), "libnew.dylib", b"NEW");
        write_dylib(dst.path(), "libuser.dylib", b"USER");

        let report = stage_plugins_from(src.path(), dst.path()).unwrap();
        assert_eq!(report.deleted, vec![dst.path().join("libold.dylib")]);
        assert!(!dst.path().join("libold.dylib").exists());
        assert!(dst.path().join("libnew.dylib").exists());
        assert!(dst.path().join("libuser.dylib").exists());
    }

    #[test]
    fn stage_does_not_remove_libraries_when_manifest_is_missing_or_invalid() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_dylib(src.path(), "libmanaged.dylib", b"LOOKBACK");
        stage_plugins_from(src.path(), dst.path()).unwrap();
        std::fs::remove_file(src.path().join("libmanaged.dylib")).unwrap();
        std::fs::remove_file(dst.path().join(MANIFEST_FILE_NAME)).unwrap();

        let missing = stage_plugins_from(src.path(), dst.path()).unwrap();
        assert!(missing.deleted.is_empty());
        assert!(dst.path().join("libmanaged.dylib").exists());

        write_dylib(dst.path(), MANIFEST_FILE_NAME, b"not json");
        let invalid = stage_plugins_from(src.path(), dst.path()).unwrap();
        assert!(invalid.deleted.is_empty());
        assert!(dst.path().join("libmanaged.dylib").exists());
        let rewritten: PluginManifest =
            serde_json::from_slice(&std::fs::read(dst.path().join(MANIFEST_FILE_NAME)).unwrap())
                .unwrap();
        assert!(rewritten.libraries.is_empty());
    }

    #[test]
    fn stage_renamed_library_removes_old_managed_name() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_dylib(src.path(), "libbefore.dylib", b"SAME_CONTENT");
        stage_plugins_from(src.path(), dst.path()).unwrap();

        std::fs::remove_file(src.path().join("libbefore.dylib")).unwrap();
        write_dylib(src.path(), "libafter.dylib", b"SAME_CONTENT");
        let report = stage_plugins_from(src.path(), dst.path()).unwrap();

        assert_eq!(report.deleted, vec![dst.path().join("libbefore.dylib")]);
        assert!(!dst.path().join("libbefore.dylib").exists());
        assert!(dst.path().join("libafter.dylib").exists());
    }

    #[test]
    fn stage_preserves_a_replaced_managed_name_with_a_different_digest() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_dylib(src.path(), "libold.dylib", b"LOOKBACK");
        stage_plugins_from(src.path(), dst.path()).unwrap();

        std::fs::remove_file(src.path().join("libold.dylib")).unwrap();
        write_dylib(dst.path(), "libold.dylib", b"USER_REPLACEMENT");
        let report = stage_plugins_from(src.path(), dst.path()).unwrap();

        assert!(report.deleted.is_empty());
        assert_eq!(
            std::fs::read(dst.path().join("libold.dylib")).unwrap(),
            b"USER_REPLACEMENT"
        );
    }

    #[test]
    fn stage_does_not_adopt_an_identical_user_library_skipped_on_first_run() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_dylib(src.path(), "libuser.dylib", b"USER");
        write_dylib(dst.path(), "libuser.dylib", b"USER");

        let first = stage_plugins_from(src.path(), dst.path()).unwrap();
        assert_eq!(first.skipped_same, vec![dst.path().join("libuser.dylib")]);
        let first_manifest: PluginManifest =
            serde_json::from_slice(&std::fs::read(dst.path().join(MANIFEST_FILE_NAME)).unwrap())
                .unwrap();
        assert!(first_manifest.libraries.is_empty());

        std::fs::remove_file(src.path().join("libuser.dylib")).unwrap();
        let second = stage_plugins_from(src.path(), dst.path()).unwrap();
        assert!(second.deleted.is_empty());
        assert_eq!(
            std::fs::read(dst.path().join("libuser.dylib")).unwrap(),
            b"USER"
        );
    }

    #[test]
    fn stage_retains_ownership_when_stale_removal_cannot_be_completed() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_dylib(src.path(), "libold.dylib", b"LOOKBACK");
        stage_plugins_from(src.path(), dst.path()).unwrap();
        std::fs::remove_file(src.path().join("libold.dylib")).unwrap();
        std::fs::remove_file(dst.path().join("libold.dylib")).unwrap();
        std::fs::create_dir(dst.path().join("libold.dylib")).unwrap();

        let report = stage_plugins_from(src.path(), dst.path()).unwrap();
        assert!(report.deleted.is_empty());
        let manifest: PluginManifest =
            serde_json::from_slice(&std::fs::read(dst.path().join(MANIFEST_FILE_NAME)).unwrap())
                .unwrap();
        assert_eq!(manifest.libraries.len(), 1);
        assert_eq!(manifest.libraries[0].name, "libold.dylib");
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_staging_preserves_previous_manifest_and_stale_libraries() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_dylib(src.path(), "libold.dylib", b"LOOKBACK");
        stage_plugins_from(src.path(), dst.path()).unwrap();
        let manifest = std::fs::read(dst.path().join(MANIFEST_FILE_NAME)).unwrap();

        std::fs::remove_file(src.path().join("libold.dylib")).unwrap();
        std::os::unix::fs::symlink("missing.dylib", src.path().join("libbroken.dylib")).unwrap();

        let report = stage_plugins_from(src.path(), dst.path()).unwrap();
        assert!(report.deleted.is_empty());
        assert!(dst.path().join("libold.dylib").exists());
        assert_eq!(
            std::fs::read(dst.path().join(MANIFEST_FILE_NAME)).unwrap(),
            manifest
        );
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
