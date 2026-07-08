//! Stage the bundled Lindera IPADIC dictionary into the data directory so
//! the memories `front` sidecar (built with `--features lindera`) can find
//! it at `LANCE_LANGUAGE_MODEL_HOME/lindera/ipadic/config.yml`.
//!
//! Lindera reads a multi-megabyte IPADIC dictionary (3.x binary format)
//! at runtime. The binary dictionary files are NOT committed; they are
//! staged into `agent-app/dict/lindera/ipadic` at build time by
//! `scripts/build-release.sh` (lindera 3.0.7 release dictionary) and bundled as a Tauri
//! resource. This code copies them into
//! `<data>/lance_language_models/lindera/ipadic` on first launch, and
//! (re)generates `config.yml` each run with the absolute staged path so it
//! stays valid even if the data root moves.
//!
//! When the dictionary source can't be resolved (dev without the bundled
//! dict, or a stripped build), staging is skipped and the caller falls back
//! to the ngram FTS tokenizer — Japanese 2-gram partial match still works,
//! just without morphological analysis (spec §3.R3).
//!
//! Source priority mirrors `plugins::resolve_plugins_source`:
//!   1. `LOOKBACK_LINDERA_SRC` env override.
//!   2. `<app>/Contents/Resources/dict/lindera/ipadic`.
//!   3. `<CARGO_MANIFEST_DIR>/../dict/lindera/ipadic` (dev).

use std::path::{Path, PathBuf};

use tauri::AppHandle;
use tracing::{debug, info, warn};

use crate::data::fsutil::{copy_atomic, file_matches};
use crate::error::{AppError, AppResult};

/// The 3.x IPADIC binary files lance-index's Lindera loader expects.
/// `metadata.json` is required by this format. `config.yml` is generated,
/// not copied.
const DICT_FILES: &[&str] = &[
    "char_def.bin",
    "dict.da",
    "dict.vals",
    "dict.words",
    "dict.wordsidx",
    "matrix.mtx",
    "metadata.json",
    "unk.bin",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinderaStageReport {
    pub source: PathBuf,
    pub copied: Vec<String>,
    pub skipped_same: Vec<String>,
    /// Absolute path to the generated `config.yml`.
    pub config_path: PathBuf,
}

/// Resolve the bundled dictionary and stage it into `dest_ipadic`
/// (`<data>/lance_language_models/lindera/ipadic`). Returns `Ok(None)` when
/// no source is available so the caller can degrade to ngram instead of
/// failing startup.
pub fn stage_lindera_dict(
    app: &AppHandle,
    dest_ipadic: &Path,
) -> AppResult<Option<LinderaStageReport>> {
    let Some(source) = resolve_lindera_source(app) else {
        return Ok(None);
    };
    stage_lindera_from(&source, dest_ipadic).map(Some)
}

/// Copy the dictionary files and (re)write `config.yml`. Pure of Tauri so
/// it's unit-testable with a temp source dir.
pub fn stage_lindera_from(source: &Path, dest_ipadic: &Path) -> AppResult<LinderaStageReport> {
    if !source.exists() {
        return Err(AppError::Config(format!(
            "lindera dict source not found: {}",
            source.display()
        )));
    }
    std::fs::create_dir_all(dest_ipadic)?;

    let mut copied = Vec::new();
    let mut skipped_same = Vec::new();
    for name in DICT_FILES {
        let src = source.join(name);
        if !src.exists() {
            // A missing core file means the bundled dict is incomplete;
            // surface it so the caller can warn rather than ship a half
            // dictionary that fails opaquely at FTS-index time.
            return Err(AppError::Config(format!(
                "lindera dict file missing from source: {}",
                src.display()
            )));
        }
        let dst = dest_ipadic.join(name);
        match file_matches(&src, &dst) {
            Ok(true) => {
                debug!(file = name, "lindera dict file unchanged, skipping");
                skipped_same.push((*name).to_string());
            }
            Ok(false) => {
                copy_atomic(&src, &dst)?;
                info!(file = %dst.display(), "lindera dict file staged");
                copied.push((*name).to_string());
            }
            Err(e) => {
                warn!(file = name, error = %e, "lindera dict hash failed");
            }
        }
    }

    // Copy the license alongside the dictionary (NAIST/ICOT terms require
    // the notice travel with redistributed copies).
    let license_src = source.join("COPYING");
    if license_src.exists() {
        let _ = std::fs::copy(&license_src, dest_ipadic.join("COPYING"));
    }

    let config_path = dest_ipadic.join("config.yml");
    std::fs::write(&config_path, render_config_yml(dest_ipadic))?;

    Ok(LinderaStageReport {
        source: source.to_path_buf(),
        copied,
        skipped_same,
        config_path,
    })
}

/// `lance-index`'s `LinderaBuilder::load` reads `config.yml` and passes
/// `segmenter.dictionary` directly to Lindera. Generated fresh each run so
/// a moved data root never leaves a stale absolute path behind.
fn render_config_yml(dict_dir: &Path) -> String {
    format!(
        "segmenter:\n  mode: \"normal\"\n  dictionary: \"{}\"\n",
        dict_dir.display()
    )
}

/// Resolve the bundled IPADIC dir. Returns `None` (not `Err`) when nothing
/// is found — Lindera is optional; the sidecar degrades to ngram.
fn resolve_lindera_source(app: &AppHandle) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LOOKBACK_LINDERA_SRC") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
        warn!(path = %pb.display(), "LOOKBACK_LINDERA_SRC set but missing");
    }
    if let Some(dir) = crate::data::paths::bundled_resource_path(app, "dict/lindera/ipadic") {
        return Some(dir);
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let candidate = PathBuf::from(manifest_dir).join("../dict/lindera/ipadic");
    let canonical = candidate.canonicalize().unwrap_or(candidate);
    canonical.exists().then_some(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_file(dir: &Path, name: &str, contents: &[u8]) {
        let mut f = std::fs::File::create(dir.join(name)).unwrap();
        f.write_all(contents).unwrap();
    }

    fn full_dict_source() -> tempfile::TempDir {
        let src = tempdir().unwrap();
        for name in DICT_FILES {
            write_file(src.path(), name, name.as_bytes());
        }
        src
    }

    #[test]
    fn stage_copies_all_dict_files_and_writes_config() {
        let src = full_dict_source();
        let dst = tempdir().unwrap();
        let ipadic = dst.path().join("lindera/ipadic");

        let report = stage_lindera_from(src.path(), &ipadic).unwrap();
        assert_eq!(report.copied.len(), DICT_FILES.len());
        assert!(report.skipped_same.is_empty());
        for name in DICT_FILES {
            assert!(ipadic.join(name).exists(), "{name} not staged");
        }
        // config.yml points at the absolute staged dir.
        let cfg = std::fs::read_to_string(ipadic.join("config.yml")).unwrap();
        assert!(cfg.contains(&ipadic.display().to_string()));
        assert!(cfg.contains("mode: \"normal\""));
        assert!(cfg.contains("dictionary: \""));
        assert!(!cfg.contains("path:"));
    }

    #[test]
    fn stage_skips_unchanged_files_on_second_run() {
        let src = full_dict_source();
        let dst = tempdir().unwrap();
        let ipadic = dst.path().join("lindera/ipadic");

        stage_lindera_from(src.path(), &ipadic).unwrap();
        let second = stage_lindera_from(src.path(), &ipadic).unwrap();
        assert!(second.copied.is_empty());
        assert_eq!(second.skipped_same.len(), DICT_FILES.len());
    }

    #[test]
    fn stage_errors_when_a_core_file_missing() {
        let src = tempdir().unwrap();
        // Only one of the required files present.
        write_file(src.path(), "char_def.bin", b"x");
        let dst = tempdir().unwrap();
        let err = stage_lindera_from(src.path(), &dst.path().join("ipadic")).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    #[test]
    fn stage_requires_lindera_3_metadata() {
        let src = full_dict_source();
        std::fs::remove_file(src.path().join("metadata.json")).unwrap();
        let dst = tempdir().unwrap();
        let err = stage_lindera_from(src.path(), &dst.path().join("ipadic")).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    #[test]
    fn stage_copies_license_when_present() {
        let src = full_dict_source();
        write_file(src.path(), "COPYING", b"NAIST/ICOT terms");
        let dst = tempdir().unwrap();
        let ipadic = dst.path().join("lindera/ipadic");
        stage_lindera_from(src.path(), &ipadic).unwrap();
        assert!(ipadic.join("COPYING").exists());
    }

    #[test]
    fn render_config_yml_embeds_absolute_path() {
        let yml = render_config_yml(Path::new("/data/lindera/ipadic"));
        assert!(yml.contains("dictionary: \"/data/lindera/ipadic\""));
        assert!(!yml.contains("path:"));
    }
}
