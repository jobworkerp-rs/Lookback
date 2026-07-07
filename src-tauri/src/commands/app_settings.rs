//! User-editable app settings (App data dir override, HF_HOME mode).
//!
//! Mirrors the persistence pattern of `connection.rs` / `llm_settings.rs`:
//! a small JSON file under the data root for the HF_HOME mode + custom
//! path, plus a separate `bootstrap.json` at the OS-default location for
//! the data-root override (read BEFORE the data root is known).
//!
//! Why two files:
//! - `<data>/app-settings.json` lives alongside the running data and is
//!   swept by `purge_all_data`. Changing the HF_HOME mode only requires a
//!   sidecar restart, so the read-on-spawn pattern in
//!   `Sidecars::start_inner` picks it up automatically.
//! - `<os-default>/bootstrap.json` survives a purge intentionally — if it
//!   pointed at the data root we'd lose the override when the user wipes
//!   their data. Changing the override requires an app relaunch (sqlite /
//!   LanceDB / tonic channels are bound to the data root at startup), so
//!   the command only writes the file and leaves restart to the user.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::paths::{self, AppSettings, DataPaths, HfHomeMode, bootstrap_path, default_root};
use crate::error::{AppError, AppResult};

use super::AppState;

/// Snapshot exposed to the Settings UI. Carries both the persisted values
/// AND the resolved paths the user actually sees in effect right now —
/// the UI shows the preview "your sidecar will use …" without re-running
/// the resolver itself.
#[derive(Debug, Clone, Serialize)]
pub struct AppSettingsResponse {
    pub hf_home_mode: HfHomeMode,
    pub hf_home_path: Option<PathBuf>,
    /// `data_root_override` from `bootstrap.json` — i.e. the data root
    /// that will be used on the NEXT launch. `None` = OS default.
    pub data_root_override: Option<PathBuf>,
    /// Explicit IANA timezone the user selected, or `None` for "Auto"
    /// (follow env `TZ` / the OS zone). Drives the Settings card's select.
    pub timezone: Option<String>,
    /// The zone the sidecar will actually inject as `TZ` right now — the
    /// resolved value of `timezone` (or, when `None`, the env/OS fallback).
    /// Shown as the card's "current effective" preview so an Auto selection
    /// still tells the user which zone is in force.
    pub effective_timezone: String,
    /// Resolved paths for the UI preview.
    pub resolved: ResolvedAppPaths,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedAppPaths {
    pub current_data_root: PathBuf,
    pub default_data_root: PathBuf,
    pub effective_hf_home: PathBuf,
    /// What the next launch will resolve to (different from
    /// `current_data_root` iff `data_root_override` was just changed and
    /// the app hasn't been relaunched yet).
    pub pending_data_root: PathBuf,
}

/// Validation outcome for a candidate data-root path. Used to give the
/// Settings UI an inline message before the user hits Save.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DataRootValidation {
    pub ok: bool,
    pub writable: bool,
    pub is_existing_lookback_root: bool,
    /// Distinguishes "absolute path, parent dir exists, just the leaf is
    /// missing" from other failure modes — the UI shows a "create" button
    /// only in this case so the user can opt into `mkdir -p` without
    /// accidentally creating a typo'd path. Mutually exclusive with `ok`.
    pub creatable: bool,
    /// i18n key for the inline UI message (or `None` if `ok`), resolved on
    /// the frontend via `t()`. NOT a human-readable string — the strict
    /// save-time guards map it through [`validation_error_text`] before
    /// embedding it in an error, so the key never leaks to a raw error toast.
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetHfHomeRequest {
    pub mode: HfHomeMode,
    #[serde(default)]
    pub path: Option<PathBuf>,
}

/// Request to set the workflow timezone. `timezone: None` (or empty) means
/// "Auto" — clear the explicit selection and follow env `TZ` / the OS zone.
#[derive(Debug, Clone, Deserialize)]
pub struct SetTimezoneRequest {
    #[serde(default)]
    pub timezone: Option<String>,
}

#[tauri::command]
pub fn get_app_settings(state: tauri::State<'_, AppState>) -> AppResult<AppSettingsResponse> {
    let app_settings = paths::load_app_settings(&state.data.app_settings_path());
    let data_root_override = paths::load_bootstrap_config(&bootstrap_path()?).data_root_override;
    let default = default_root()?;
    // `pending_data_root` must apply the SAME validity gate as
    // `DataPaths::resolve` (absolute + currently-existing directory),
    // not just echo the raw `data_root_override`. Otherwise the Settings
    // UI tells the user "next launch will use /Volumes/Ext/lookback"
    // while the next launch will actually fall back to the default —
    // e.g. the external disk is currently unplugged, or a purge wiped
    // the override target. Same helper drives both paths so they can't
    // drift.
    let pending = paths::resolved_data_root(&default, data_root_override.as_deref());
    let timezone = app_settings.timezone.clone();
    // Preview of the zone the sidecar will inject NOW (honours an explicit
    // selection, else the env/OS fallback) — the same resolver the sidecar
    // spawn uses, so the card can never disagree with reality.
    let effective_timezone = crate::sidecar::lifecycle::resolve_timezone(Some(&app_settings));
    Ok(AppSettingsResponse {
        hf_home_mode: app_settings.hf_home_mode,
        hf_home_path: app_settings.hf_home_path,
        data_root_override,
        timezone,
        effective_timezone,
        resolved: ResolvedAppPaths {
            current_data_root: state.data.root.clone(),
            default_data_root: default,
            effective_hf_home: state.sidecars.effective_hf_home(),
            pending_data_root: pending,
        },
    })
}

/// Persist the generation output language to `app-settings.json` so headless
/// paths (conductor periodic runs) can read the UI's current language. The
/// frontend calls this whenever the locale changes, mirroring how it persists
/// the locale to `localStorage` for the UI itself. Per-dispatch commands still
/// pass an explicit value that takes precedence (see `resolve_output_language`).
///
/// Whitelist-validated against `SUPPORTED_LANGUAGES`; an unsupported value is
/// rejected rather than silently persisted (it would dangle against a worker
/// that was never registered).
#[tauri::command]
pub fn set_output_language(state: tauri::State<'_, AppState>, lang: String) -> AppResult<()> {
    let trimmed = lang.trim();
    if !super::SUPPORTED_LANGUAGES.contains(&trimmed) {
        return Err(AppError::Config(format!(
            "unsupported output language `{trimmed}`; expected one of {:?}",
            super::SUPPORTED_LANGUAGES
        )));
    }
    let path = state.data.app_settings_path();
    let mut cfg = paths::load_app_settings(&path);
    cfg.output_language = Some(trimmed.to_string());
    paths::save_app_settings(&path, &cfg)
}

/// Set the App data dir override. Persists to `bootstrap.json` only —
/// the running AppState / sqlite / LanceDB / tonic channels stay bound to
/// the current root because dynamic switching would require a full
/// teardown of every long-lived resource. The frontend reads the
/// "pending" field to tell the user the change will take effect on the
/// next launch.
///
/// `path = None` clears the override (i.e. revert to OS default).
#[tauri::command]
pub fn set_data_root(path: Option<String>) -> AppResult<()> {
    let normalised = path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    if let Some(p) = normalised.as_ref() {
        validate_data_root_path(p)?;
    }
    let bootstrap_path = bootstrap_path()?;
    save_data_root_override(&bootstrap_path, normalised)
}

fn save_data_root_override(path: &Path, value: Option<PathBuf>) -> AppResult<()> {
    let mut config = paths::load_bootstrap_config(path);
    config.data_root_override = value;
    paths::save_bootstrap_config(path, &config)
}

/// Set the HF_HOME mode. Persists to `<data>/app-settings.json` and
/// restarts the sidecars so the new value reaches the jobworkerp child
/// via env (the read-on-spawn pattern in `Sidecars::start_inner` re-reads
/// the file on each restart).
/// Validate and persist `app-settings.json` (HF_HOME) — WITHOUT
/// restarting the sidecar. Returns whether the value actually changed
/// (so a no-op save skips the restart).
///
/// Split out from [`set_hf_home`] so the unified `apply_settings` command
/// can persist several settings and restart the sidecar exactly once.
/// Validate an HF_HOME request WITHOUT persisting. Split out so the
/// unified `apply_settings` can validate the whole batch before any file
/// is written.
pub fn validate_hf_home_request(request: &SetHfHomeRequest) -> AppResult<()> {
    if matches!(request.mode, HfHomeMode::Custom) {
        let path = request
            .path
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| AppError::Config("custom HF_HOME requires a path".into()))?;
        validate_hf_home_custom_path(path)?;
    }
    Ok(())
}

pub fn apply_hf_home_to_disk(data: &DataPaths, request: SetHfHomeRequest) -> AppResult<bool> {
    // Validate before persisting so a typo doesn't leave the sidecar
    // pointing at an unusable HF_HOME on the next restart.
    validate_hf_home_request(&request)?;

    let app_settings_path = data.app_settings_path();
    let old = paths::load_app_settings(&app_settings_path);
    let cfg = AppSettings {
        hf_home_mode: request.mode,
        hf_home_path: request.path.filter(|p| !p.as_os_str().is_empty()),
        // Preserve unrelated settings (output_language) this command doesn't own.
        ..old.clone()
    };
    let changed = old.hf_home_mode != cfg.hf_home_mode || old.hf_home_path != cfg.hf_home_path;
    paths::save_app_settings(&app_settings_path, &cfg)?;
    Ok(changed)
}

#[tauri::command]
pub async fn set_hf_home(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    request: SetHfHomeRequest,
) -> AppResult<()> {
    apply_hf_home_to_disk(&state.data, request)?;

    // Mirror `retry_model_setup`: stop sidecars, drop the cached gRPC
    // clients, then re-spawn. `start_inner` re-resolves `effective_hf_home`
    // from the just-saved file (the field is `RwLock`-backed).
    state.sidecars.stop().await?;
    state.invalidate_clients().await;
    let sidecars = state.sidecars.clone();
    let data = state.data.clone();
    crate::stage_and_start_sidecars(&app, &sidecars, &data).await;
    Ok(())
}

// ── Timezone ──────────────────────────────────────────────────────────
//
// The workflow timezone is persisted into `app-settings.json` (same file as
// HF_HOME / output_language) and injected as the jobworkerp worker's `TZ`
// env on spawn — the DST-aware boundary source the summary/import workflow
// jq reads. `TZ` is read at spawn time, so a change is applied via the
// unified `apply_settings` single-restart pipeline (hot-reload impossible,
// same category as HF_HOME / MCP). `validate_*` / `apply_*_to_disk` are split
// so `apply_settings` can validate the whole batch before writing any file.

/// The tz database directories, in preference order. macOS 10.13+ ships the
/// real zoneinfo under `/var/db/timezone/zoneinfo` (a symlink target of
/// `/etc/localtime`); most Linux distros use `/usr/share/zoneinfo`. The first
/// existing one is the source of truth for BOTH the selectable list and the
/// save-time validation, so a name that lists is always a name that validates.
const ZONEINFO_DIRS: &[&str] = &["/var/db/timezone/zoneinfo", "/usr/share/zoneinfo"];

/// Top-level tz areas we surface. Restricting to these drops the tzdb's
/// aliases / special files (`posix`, `right`, `SystemV`, `Factory`, the
/// `*.tab` indexes, bare abbreviations) so the UI list stays the canonical
/// `Area/Location` zones plus `UTC`.
const ZONEINFO_AREAS: &[&str] = &[
    "Africa",
    "America",
    "Antarctica",
    "Arctic",
    "Asia",
    "Atlantic",
    "Australia",
    "Europe",
    "Indian",
    "Pacific",
];

fn zoneinfo_root() -> Option<PathBuf> {
    ZONEINFO_DIRS.iter().map(PathBuf::from).find(|p| p.is_dir())
}

/// Test-only accessor so sibling command modules can guard-skip zoneinfo-
/// dependent assertions on hosts without a tz database.
#[cfg(test)]
pub(crate) fn zoneinfo_root_for_test() -> Option<PathBuf> {
    zoneinfo_root()
}

/// True when `name` is a syntactically safe relative IANA name — rejects
/// absolute paths, `..` traversal, and empty segments so `validate` can join
/// it onto a zoneinfo root without escaping the directory.
fn is_safe_zone_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && name
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// Validate WITHOUT persisting (split out so `apply_settings` can validate the
/// whole batch first). `None`/empty ⇒ Ok (the "Auto" selection). A non-empty
/// name must exist as a file under a zoneinfo root — the same tzdb the sidecar
/// resolves `TZ` against, so a validated name can never dangle at spawn.
pub fn validate_timezone_request(request: &SetTimezoneRequest) -> AppResult<()> {
    let Some(name) = request
        .timezone
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(()); // Auto.
    };
    if !is_safe_zone_name(name) {
        return Err(AppError::Config(format!("invalid timezone name `{name}`")));
    }
    // When no zoneinfo dir exists (unusual), accept the name rather than
    // block the user — the sidecar's own tzdb is the final arbiter, and a
    // wrong name there just falls back to UTC-as-if in the jq, not a crash.
    let Some(root) = zoneinfo_root() else {
        return Ok(());
    };
    if root.join(name).is_file() {
        Ok(())
    } else {
        Err(AppError::Config(format!(
            "unknown timezone `{name}` (not found in the tz database)"
        )))
    }
}

/// Validate + persist the timezone into `app-settings.json` WITHOUT restarting
/// the sidecar. Returns whether the value actually changed (a no-op save skips
/// the restart). Empty ⇒ `None` (Auto). `..old.clone()` preserves the
/// unrelated `hf_home_*` / `output_language` fields sharing this file.
pub fn apply_timezone_to_disk(data: &DataPaths, request: SetTimezoneRequest) -> AppResult<bool> {
    validate_timezone_request(&request)?;
    let normalized = request
        .timezone
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let app_settings_path = data.app_settings_path();
    let old = paths::load_app_settings(&app_settings_path);
    let cfg = AppSettings {
        timezone: normalized,
        ..old.clone()
    };
    let changed = old.timezone != cfg.timezone;
    paths::save_app_settings(&app_settings_path, &cfg)?;
    Ok(changed)
}

/// List the selectable IANA timezone names from the host tz database. Walks
/// the first existing zoneinfo root, keeps only the `Area/Location` zones
/// under [`ZONEINFO_AREAS`] plus `UTC`, and returns them sorted. Empty when no
/// zoneinfo dir exists — the frontend then offers only "Auto" + free text.
#[tauri::command]
pub fn list_timezones() -> AppResult<Vec<String>> {
    let Some(root) = zoneinfo_root() else {
        return Ok(Vec::new());
    };
    let mut zones = Vec::new();
    // `UTC` is a top-level file, not under an area prefix.
    if root.join("UTC").is_file() {
        zones.push("UTC".to_string());
    }
    for area in ZONEINFO_AREAS {
        collect_zone_names(&root.join(area), area, &mut zones);
    }
    zones.sort();
    Ok(zones)
}

/// Recurse `dir`, appending `prefix/<file>` for each regular file (zones can
/// nest, e.g. `America/Argentina/Buenos_Aires`).
fn collect_zone_names(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let path = entry.path();
        let child_prefix = format!("{prefix}/{name}");
        if path.is_dir() {
            collect_zone_names(&path, &child_prefix, out);
        } else if path.is_file() {
            out.push(child_prefix);
        }
    }
}

/// Set the workflow timezone. Thin wrapper that delegates to the unified
/// `apply_settings` pipeline (single validate → persist → restart), mirroring
/// `set_mcp_settings`. `TZ` is spawn-time env, so this always restarts.
#[tauri::command]
pub async fn set_timezone(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    request: SetTimezoneRequest,
) -> AppResult<super::apply_settings::ApplySettingsResponse> {
    super::apply_settings::apply_settings(
        app,
        state,
        super::apply_settings::ApplySettingsRequest {
            llm: None,
            embedding: None,
            hf_home: None,
            mcp: None,
            timezone: Some(request),
        },
    )
    .await
}

/// Pre-flight check for a candidate data-root path. Returns a structured
/// outcome the UI can render inline. Thin wrapper around
/// [`validate_data_root_impl`] so the impl can be called internally with
/// a `&Path` (avoiding the String round-trip) by `set_data_root` /
/// `create_data_root` while the Tauri command keeps a `String` signature
/// the frontend can `invoke`.
#[tauri::command]
pub fn validate_data_root(path: String) -> AppResult<DataRootValidation> {
    Ok(validate_data_root_impl(&PathBuf::from(path.trim())))
}

/// Internal validation core. Shared by the Tauri command, the strict
/// `set_data_root` guard, and `create_data_root`'s pre-mkdir check so the
/// rules can't drift between paths.
fn validate_data_root_impl(p: &Path) -> DataRootValidation {
    if p.as_os_str().is_empty() {
        return reject("settings.dataRoot.validation.empty");
    }
    if !p.is_absolute() {
        return reject("settings.dataRoot.validation.notAbsolute");
    }
    if !p.exists() {
        // Surface "creatable" only when the parent itself exists and is
        // writable — otherwise the create button would just trade one
        // failure for another, and a wildly wrong path (e.g. a typo'd
        // mount point) would silently materialise an unwanted directory.
        return DataRootValidation {
            ok: false,
            writable: false,
            is_existing_lookback_root: false,
            creatable: paths::parent_is_writable(p),
            // i18n key; the frontend resolves the message via `t()`.
            message: Some("settings.dataRoot.validation.notExist".into()),
        };
    }
    if !p.is_dir() {
        return reject("settings.dataRoot.validation.notDir");
    }
    if !paths::is_writable(p) {
        return reject("settings.dataRoot.validation.notWritable");
    }
    let is_existing = crate::data::DataPaths::looks_like_existing_root(p);
    DataRootValidation {
        ok: true,
        writable: true,
        is_existing_lookback_root: is_existing,
        creatable: false,
        // i18n key; the frontend resolves the message via `t()`.
        message: is_existing.then(|| "settings.dataRoot.validation.existingRoot".into()),
    }
}

/// `msg` is an i18n key (e.g. `settings.dataRoot.validation.empty`), not a
/// localized string — the frontend resolves it via `t()`.
fn reject(msg: &str) -> DataRootValidation {
    DataRootValidation {
        ok: false,
        writable: false,
        is_existing_lookback_root: false,
        creatable: false,
        message: Some(msg.into()),
    }
}

/// Human-readable (English) reason for a validation `message` i18n key.
///
/// `DataRootValidation.message` is an i18n key meant for the UI, which the
/// frontend resolves via `t()`. The strict save-time guards, by contrast,
/// embed the reason into an `AppError::Config` string that the frontend
/// surfaces verbatim (no `t()`), so they must NOT leak the raw key. This maps
/// each known key to a stable English phrase; an unknown key falls back to a
/// generic message rather than echoing the key.
fn validation_error_text(message: Option<&str>) -> &'static str {
    match message {
        Some("settings.dataRoot.validation.empty") => "path is empty",
        Some("settings.dataRoot.validation.notAbsolute") => "path must be absolute",
        Some("settings.dataRoot.validation.notExist") => "directory does not exist",
        Some("settings.dataRoot.validation.notDir") => "not a directory",
        Some("settings.dataRoot.validation.notWritable") => "no write permission",
        _ => "validation failed",
    }
}

/// Create a candidate data-root directory (recursive `mkdir -p`) so the
/// user can opt into a fresh path from Settings without dropping to a
/// terminal. The same validation as the UI's create-button guard runs
/// before mkdir so a typo'd path can't silently materialise. Idempotent:
/// an already-existing valid root is treated as success.
#[tauri::command]
pub fn create_data_root(path: String) -> AppResult<()> {
    let p = PathBuf::from(path.trim());
    let v = validate_data_root_impl(&p);
    // Two acceptable shapes: (a) already exists & passes validation,
    // (b) missing but the parent is writable. Anything else is a typo.
    if !v.ok && !v.creatable {
        return Err(AppError::Config(format!(
            "invalid data root {}: {}",
            p.display(),
            validation_error_text(v.message.as_deref())
        )));
    }
    std::fs::create_dir_all(&p)
        .map_err(|e| AppError::Config(format!("ディレクトリ作成失敗 {}: {e}", p.display())))?;
    Ok(())
}

/// Validate a candidate custom HF_HOME before `set_hf_home` persists it.
///
/// HF_HOME differs from the data root in that HuggingFace Hub creates
/// missing subdirs on first use, so we accept a non-existent path AS
/// LONG AS the parent is writable. But an existing path MUST be a
/// directory (a regular file at the same path would make llama.cpp /
/// hf-xet fail mid-download with a `Not a directory` errno that surfaces
/// far from Settings) and writable (HF Hub needs to populate it).
fn validate_hf_home_custom_path(path: &Path) -> AppResult<()> {
    if !path.is_absolute() {
        return Err(AppError::Config(format!(
            "HF_HOME path must be absolute: {}",
            path.display()
        )));
    }
    if path.exists() {
        if !path.is_dir() {
            return Err(AppError::Config(format!(
                "HF_HOME path must be a directory, not a file: {}",
                path.display()
            )));
        }
        if !paths::is_writable(path) {
            return Err(AppError::Config(format!(
                "HF_HOME path is not writable: {}",
                path.display()
            )));
        }
        return Ok(());
    }
    // Non-existent: HF Hub will create it on first download, but only if
    // the parent exists and is writable. Otherwise the first model fetch
    // fails deep inside the sidecar instead of at save time.
    if !paths::parent_is_writable(path) {
        return Err(AppError::Config(format!(
            "HF_HOME parent directory does not exist or is not writable: {}",
            path.display()
        )));
    }
    Ok(())
}

/// Strict variant used by `set_data_root` so a typo doesn't reach
/// `bootstrap.json`. Calls the impl directly (no command round-trip).
fn validate_data_root_path(p: &Path) -> AppResult<()> {
    let v = validate_data_root_impl(p);
    if v.ok {
        return Ok(());
    }
    Err(AppError::Config(format!(
        "invalid data root {}: {}",
        p.display(),
        validation_error_text(v.message.as_deref())
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn validate_data_root_rejects_empty() {
        let v = validate_data_root("".into()).unwrap();
        assert!(!v.ok);
        assert!(!v.writable);
    }

    #[test]
    fn validate_data_root_rejects_relative_path() {
        let v = validate_data_root("relative/path".into()).unwrap();
        assert!(!v.ok);
    }

    #[test]
    fn validate_data_root_rejects_nonexistent() {
        let v = validate_data_root("/tmp/lookback-nonexistent-xxxxxx".into()).unwrap();
        assert!(!v.ok);
    }

    #[test]
    fn is_writable_does_not_clobber_user_files_with_matching_probe_name() {
        // Critical safety property: validate_data_root is fired on every
        // 300ms input debounce against user-supplied directories. A naive
        // probe that did `fs::write` followed by `fs::remove_file` would
        // truncate THEN delete any pre-existing file at the probe path,
        // turning Settings into a silent file-shredder for unlucky names.
        let dir = tempdir();
        // Plant a fake "previous probe leftover" with content. The new
        // probe must NOT touch it, even though it sits under the same
        // .lookback-write-probe-* convention.
        let planted = dir.path().join(format!(
            ".lookback-write-probe-{}-{}",
            std::process::id(),
            0
        ));
        std::fs::write(&planted, b"user data").unwrap();

        assert!(paths::is_writable(dir.path()));

        // The planted file's content must be intact and the file still
        // present — the probe walked around it via create_new + a unique
        // nanos suffix.
        assert!(planted.exists(), "probe deleted an unrelated file");
        assert_eq!(std::fs::read(&planted).unwrap(), b"user data");
    }

    #[test]
    fn is_writable_does_not_leave_probe_files_behind() {
        // The probe must be removed on success so a successful validate
        // doesn't leave litter under the user's chosen directory.
        let dir = tempdir();
        assert!(paths::is_writable(dir.path()));
        // Directory should be empty again (the probe cleaned up after
        // itself). No iteration over file names — we just assert nothing
        // was added.
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(
            entries.is_empty(),
            "probe left {} files behind: {:?}",
            entries.len(),
            entries
                .into_iter()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn validate_data_root_flags_creatable_when_only_leaf_is_missing() {
        // The parent (a fresh tempdir) exists and is writable, so the UI
        // can offer a "create" button to materialise the leaf.
        let dir = tempdir();
        let leaf = dir.path().join("does-not-exist-yet");
        let v = validate_data_root(leaf.display().to_string()).unwrap();
        assert!(!v.ok);
        assert!(v.creatable);
    }

    #[test]
    fn validate_data_root_does_not_flag_creatable_when_parent_missing() {
        // Without a writable parent, offering "create" would silently
        // materialise a deeply-nested phantom tree under whatever first
        // existing ancestor we hit. Force the user to fix the parent
        // first.
        let v = validate_data_root("/tmp/lookback-no-parent-xxxxxx/leaf".into()).unwrap();
        assert!(!v.ok);
        assert!(!v.creatable);
    }

    #[test]
    fn create_data_root_creates_missing_directory() {
        let dir = tempdir();
        let leaf = dir.path().join("new-root");
        assert!(!leaf.exists());
        create_data_root(leaf.display().to_string()).unwrap();
        assert!(leaf.is_dir());
    }

    #[test]
    fn create_data_root_is_idempotent_for_existing_directory() {
        let dir = tempdir();
        // Calling twice must not fail — the user may have hit the button
        // and then re-validated, and we shouldn't surface an error for
        // "already exists" since that's the desired end state.
        create_data_root(dir.path().display().to_string()).unwrap();
        create_data_root(dir.path().display().to_string()).unwrap();
    }

    #[test]
    fn create_data_root_rejects_relative_path() {
        let err = create_data_root("relative/dir".into()).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    #[test]
    fn create_data_root_rejects_path_with_missing_parent() {
        // Mirrors the `creatable=false` guard in validate_data_root: a
        // wildly wrong path (parent doesn't exist) must NOT materialise
        // a phantom tree. The user has to fix the parent first.
        let err = create_data_root("/tmp/lookback-phantom-parent-xxxxxx/leaf".into()).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    #[test]
    fn validate_hf_home_rejects_existing_regular_file() {
        // Critical regression: previously the only checks were "absolute"
        // and "parent exists", so a user could type /Users/me/notes.txt
        // and have it accepted. The sidecar then failed at first model
        // download with a misleading "Not a directory" deep in the
        // jobworkerp log. Catch it at save time.
        let dir = tempdir();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, b"user notes").unwrap();
        let err = validate_hf_home_custom_path(&file).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    #[test]
    fn validate_hf_home_accepts_existing_writable_directory() {
        let dir = tempdir();
        validate_hf_home_custom_path(dir.path()).unwrap();
    }

    #[test]
    fn validate_hf_home_accepts_nonexistent_path_with_writable_parent() {
        // HF Hub creates the cache root on first download, so a
        // non-existent leaf is fine as long as the parent is writable.
        let dir = tempdir();
        let leaf = dir.path().join("hf-cache");
        validate_hf_home_custom_path(&leaf).unwrap();
    }

    #[test]
    fn validate_hf_home_rejects_nonexistent_with_missing_parent() {
        let err = validate_hf_home_custom_path(Path::new("/tmp/lookback-no-such-parent-xxx/hf"))
            .unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    #[test]
    fn validate_hf_home_rejects_relative_path() {
        let err = validate_hf_home_custom_path(Path::new("relative/hf")).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    #[test]
    fn validate_data_root_accepts_writable_dir() {
        let dir = tempdir();
        let v = validate_data_root(dir.path().display().to_string()).unwrap();
        assert!(v.ok, "validation failed: {:?}", v.message);
        assert!(v.writable);
        assert!(!v.is_existing_lookback_root);
    }

    #[test]
    fn validate_data_root_recognises_existing_lookback_root_by_sqlite() {
        let dir = tempdir();
        let db = dir.path().join("db");
        std::fs::create_dir_all(&db).unwrap();
        std::fs::write(db.join("jobworkerp.sqlite3"), b"").unwrap();
        let v = validate_data_root(dir.path().display().to_string()).unwrap();
        assert!(v.ok);
        assert!(v.is_existing_lookback_root);
    }

    #[test]
    fn validate_data_root_recognises_existing_lookback_root_by_lancedb() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join("lancedb")).unwrap();
        let v = validate_data_root(dir.path().display().to_string()).unwrap();
        assert!(v.ok);
        assert!(v.is_existing_lookback_root);
    }

    #[test]
    fn validate_data_root_path_strict_errors_on_typo() {
        let err = validate_data_root_path(Path::new("/tmp/lookback-typo-xxxxx")).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    #[test]
    fn strict_guard_error_is_human_readable_not_an_i18n_key() {
        // Regression: `message` carries an i18n KEY for the UI, but the
        // save-time guard's error is shown verbatim (no `t()`) by the
        // frontend toast. A relative path is rejected with `notAbsolute`;
        // the error must read as prose, never echo the raw key.
        let AppError::Config(msg) =
            validate_data_root_path(Path::new("relative/path")).unwrap_err()
        else {
            panic!("expected AppError::Config");
        };
        assert!(
            !msg.contains("settings.dataRoot.validation"),
            "strict guard leaked an i18n key into the error: {msg}"
        );
        assert!(
            msg.contains("path must be absolute"),
            "strict guard error should be human-readable: {msg}"
        );
    }

    #[test]
    fn validation_error_text_covers_every_reject_key() {
        // Every key `reject` / `validate_data_root_impl` can emit must map to
        // a non-generic phrase, so a save-time error is never the fallback
        // "validation failed" when a specific reason is known.
        for key in [
            "settings.dataRoot.validation.empty",
            "settings.dataRoot.validation.notAbsolute",
            "settings.dataRoot.validation.notExist",
            "settings.dataRoot.validation.notDir",
            "settings.dataRoot.validation.notWritable",
        ] {
            assert_ne!(
                validation_error_text(Some(key)),
                "validation failed",
                "key {key} fell through to the generic fallback"
            );
        }
        // `existingRoot` is an `ok` outcome (never reaches an error path), and
        // `None` / unknown keys fall back to the generic message.
        assert_eq!(validation_error_text(None), "validation failed");
    }

    // ── apply_hf_home_to_disk ─────────────────────────────────────────

    #[test]
    fn apply_hf_home_to_disk_persists_and_reports_change() {
        let dir = tempdir();
        let data = DataPaths::with_root(dir.path().to_path_buf());
        // Default is Global; switching to DataRoot is a change.
        let changed = apply_hf_home_to_disk(
            &data,
            SetHfHomeRequest {
                mode: HfHomeMode::DataRoot,
                path: None,
            },
        )
        .unwrap();
        assert!(changed);
        let saved = paths::load_app_settings(&data.app_settings_path());
        assert_eq!(saved.hf_home_mode, HfHomeMode::DataRoot);
    }

    #[test]
    fn apply_hf_home_to_disk_noop_reports_no_change() {
        let dir = tempdir();
        let data = DataPaths::with_root(dir.path().to_path_buf());
        apply_hf_home_to_disk(
            &data,
            SetHfHomeRequest {
                mode: HfHomeMode::DataRoot,
                path: None,
            },
        )
        .unwrap();
        let changed = apply_hf_home_to_disk(
            &data,
            SetHfHomeRequest {
                mode: HfHomeMode::DataRoot,
                path: None,
            },
        )
        .unwrap();
        assert!(!changed, "re-saving the same mode must not report a change");
    }

    #[test]
    fn apply_hf_home_to_disk_rejects_custom_without_path() {
        let dir = tempdir();
        let data = DataPaths::with_root(dir.path().to_path_buf());
        let err = apply_hf_home_to_disk(
            &data,
            SetHfHomeRequest {
                mode: HfHomeMode::Custom,
                path: None,
            },
        );
        assert!(err.is_err(), "custom mode requires a path");
    }

    #[test]
    fn save_data_root_override_preserves_setup_completion() {
        let dir = tempdir();
        let path = dir.path().join("bootstrap.json");
        paths::save_bootstrap_config(
            &path,
            &paths::BootstrapConfig {
                data_root_override: None,
                setup_completed: true,
            },
        )
        .unwrap();

        save_data_root_override(&path, Some(PathBuf::from("/tmp/lookback-next"))).unwrap();

        let saved = paths::load_bootstrap_config(&path);
        assert!(saved.setup_completed);
        assert_eq!(
            saved.data_root_override,
            Some(PathBuf::from("/tmp/lookback-next"))
        );
    }

    // ── Timezone ──

    fn tz_req(tz: Option<&str>) -> SetTimezoneRequest {
        SetTimezoneRequest {
            timezone: tz.map(str::to_string),
        }
    }

    #[test]
    fn validate_timezone_request_accepts_none_as_auto() {
        assert!(validate_timezone_request(&tz_req(None)).is_ok());
        assert!(validate_timezone_request(&tz_req(Some(""))).is_ok());
        assert!(validate_timezone_request(&tz_req(Some("   "))).is_ok());
    }

    #[test]
    fn validate_timezone_request_accepts_known_zone() {
        // Guard-skip on hosts without a zoneinfo dir (unusual CI images):
        // there the validator is permissive by design, so there is nothing
        // to assert.
        if zoneinfo_root().is_none() {
            return;
        }
        assert!(validate_timezone_request(&tz_req(Some("Asia/Tokyo"))).is_ok());
        assert!(validate_timezone_request(&tz_req(Some("America/New_York"))).is_ok());
    }

    #[test]
    fn validate_timezone_request_rejects_unknown_zone() {
        if zoneinfo_root().is_none() {
            return;
        }
        assert!(validate_timezone_request(&tz_req(Some("Not/AZone"))).is_err());
    }

    #[test]
    fn validate_timezone_request_rejects_path_traversal() {
        // Rejected before any filesystem lookup, so this holds regardless of
        // whether a zoneinfo dir exists.
        assert!(validate_timezone_request(&tz_req(Some("../../etc/passwd"))).is_err());
        assert!(validate_timezone_request(&tz_req(Some("/etc/passwd"))).is_err());
        assert!(validate_timezone_request(&tz_req(Some("Asia/../../etc"))).is_err());
    }

    #[test]
    fn apply_timezone_to_disk_persists_and_reports_change() {
        let dir = tempdir();
        let data = DataPaths::with_root(dir.path().to_path_buf());
        // Use a name that validates when a zoneinfo dir exists; on a host
        // without one the validator is permissive so it still persists.
        let changed = apply_timezone_to_disk(&data, tz_req(Some("Asia/Tokyo"))).unwrap();
        assert!(changed);
        let saved = paths::load_app_settings(&data.app_settings_path());
        assert_eq!(saved.timezone.as_deref(), Some("Asia/Tokyo"));
    }

    #[test]
    fn apply_timezone_to_disk_noop_reports_no_change() {
        let dir = tempdir();
        let data = DataPaths::with_root(dir.path().to_path_buf());
        apply_timezone_to_disk(&data, tz_req(Some("Asia/Tokyo"))).unwrap();
        let changed = apply_timezone_to_disk(&data, tz_req(Some("Asia/Tokyo"))).unwrap();
        assert!(!changed, "re-applying the same zone must be a no-op");
    }

    #[test]
    fn apply_timezone_to_disk_normalizes_empty_to_auto() {
        let dir = tempdir();
        let data = DataPaths::with_root(dir.path().to_path_buf());
        apply_timezone_to_disk(&data, tz_req(Some("Asia/Tokyo"))).unwrap();
        // An empty selection clears back to Auto (None).
        let changed = apply_timezone_to_disk(&data, tz_req(Some("  "))).unwrap();
        assert!(changed);
        let saved = paths::load_app_settings(&data.app_settings_path());
        assert_eq!(saved.timezone, None);
    }

    #[test]
    fn apply_timezone_to_disk_preserves_unrelated_fields() {
        let dir = tempdir();
        let data = DataPaths::with_root(dir.path().to_path_buf());
        // Seed unrelated app-settings fields first.
        let seed = AppSettings {
            hf_home_mode: HfHomeMode::DataRoot,
            output_language: Some("en".into()),
            ..Default::default()
        };
        paths::save_app_settings(&data.app_settings_path(), &seed).unwrap();

        apply_timezone_to_disk(&data, tz_req(Some("Asia/Tokyo"))).unwrap();

        let saved = paths::load_app_settings(&data.app_settings_path());
        assert_eq!(saved.timezone.as_deref(), Some("Asia/Tokyo"));
        assert!(
            matches!(saved.hf_home_mode, HfHomeMode::DataRoot),
            "hf_home_mode must survive a timezone write"
        );
        assert_eq!(
            saved.output_language.as_deref(),
            Some("en"),
            "output_language must survive a timezone write"
        );
    }

    #[test]
    fn list_timezones_returns_nonempty_and_contains_common_zone() {
        if zoneinfo_root().is_none() {
            return; // No tzdb on this host — the list is legitimately empty.
        }
        let zones = list_timezones().unwrap();
        assert!(!zones.is_empty());
        assert!(
            zones.iter().any(|z| z == "Asia/Tokyo"),
            "expected a common zone in the list"
        );
        // The list must be the canonical Area/Location zones, not raw tzdb
        // index files.
        assert!(
            !zones
                .iter()
                .any(|z| z.ends_with(".tab") || z.contains("posix/")),
            "index / alias files must be filtered out"
        );
    }
}
