use std::path::Path;

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::data::DataPaths;
use crate::error::{AppError, AppResult};
use crate::experimental_features::ISSUE_TRACKING_BUILD_ALLOWS;

/// Process environment is shared across Rust tests; serialize tests that
/// exercise Issue Tracker endpoint inheritance and cleanup.
#[cfg(test)]
pub(crate) static ISSUE_TRACKER_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Persisted Issue Tracker feature toggle.
///
/// `None` represents an unset value and preserves the build-time default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct IssueTrackerSettings {
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Request used by the unified settings apply command.
#[derive(Debug, Clone, Deserialize)]
pub struct SetIssueTrackerSettingsRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Frontend-facing settings snapshot. `enabled` preserves the explicit value,
/// while `effective_enabled` includes the build-time fallback.
#[derive(Debug, Clone, Serialize)]
pub struct IssueTrackerSettingsResponse {
    pub enabled: Option<bool>,
    pub effective_enabled: bool,
}

/// Resolve the effective Issue Tracker state from persisted settings and the
/// build-time allowance.
pub fn resolve_issue_tracking(settings: &IssueTrackerSettings, build_allows: bool) -> bool {
    build_allows && settings.enabled.unwrap_or(true)
}

#[derive(Debug, thiserror::Error)]
enum IssueTrackerSettingsLoadError {
    #[error("failed to read issue tracker settings: {0}")]
    Read(#[source] std::io::Error),
    #[error("failed to decode issue tracker settings: {0}")]
    Decode(#[source] serde_json::Error),
}

impl IssueTrackerSettingsLoadError {
    fn should_warn(&self) -> bool {
        !matches!(
            self,
            Self::Read(error) if error.kind() == std::io::ErrorKind::NotFound
        )
    }
}

fn load_issue_tracker_settings_result(
    path: &Path,
) -> Result<IssueTrackerSettings, IssueTrackerSettingsLoadError> {
    let bytes = std::fs::read(path).map_err(IssueTrackerSettingsLoadError::Read)?;
    serde_json::from_slice(&bytes).map_err(IssueTrackerSettingsLoadError::Decode)
}

/// Load the persisted Issue Tracker settings, falling back to an unset value
/// when the file is missing or cannot be read or decoded.
pub fn load_issue_tracker_settings(path: &Path) -> IssueTrackerSettings {
    match load_issue_tracker_settings_result(path) {
        Ok(settings) => settings,
        Err(error) => {
            if error.should_warn() {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "failed to load issue tracker settings; using build allowance"
                );
            }
            IssueTrackerSettings::default()
        }
    }
}

/// Persist Issue Tracker settings, creating the parent directory as needed.
pub fn save_issue_tracker_settings(path: &Path, settings: &IssueTrackerSettings) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(settings)
        .map_err(|error| AppError::Config(format!("serialize issue tracker settings: {error}")))?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Resolve the Issue Tracker state using the persisted value under `data` and
/// the compile-time build allowance when no explicit value is saved.
pub fn effective_issue_tracking_enabled(data: &DataPaths) -> bool {
    effective_issue_tracking_enabled_with_build_allows(data, ISSUE_TRACKING_BUILD_ALLOWS)
}

pub(crate) fn issue_tracker_settings_response(
    data: &DataPaths,
    build_allows: bool,
) -> IssueTrackerSettingsResponse {
    let settings = load_issue_tracker_settings(&data.issue_tracker_settings_path());
    IssueTrackerSettingsResponse {
        enabled: settings.enabled,
        effective_enabled: resolve_issue_tracking(&settings, build_allows),
    }
}

pub(crate) fn effective_issue_tracking_enabled_with_build_allows(
    data: &DataPaths,
    build_allows: bool,
) -> bool {
    let settings = load_issue_tracker_settings(&data.issue_tracker_settings_path());
    resolve_issue_tracking(&settings, build_allows)
}

#[tauri::command]
pub fn get_issue_tracker_settings(
    state: State<'_, crate::commands::AppState>,
) -> AppResult<IssueTrackerSettingsResponse> {
    Ok(issue_tracker_settings_response(
        &state.data,
        ISSUE_TRACKING_BUILD_ALLOWS,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::DataPaths;

    #[test]
    fn resolver_uses_both_build_allowance_values_when_setting_is_unset() {
        let settings = IssueTrackerSettings { enabled: None };

        assert!(!resolve_issue_tracking(&settings, false));
        assert!(resolve_issue_tracking(&settings, true));
    }

    #[test]
    fn resolver_prefers_explicit_values_in_both_directions() {
        let enabled = IssueTrackerSettings {
            enabled: Some(true),
        };
        let disabled = IssueTrackerSettings {
            enabled: Some(false),
        };

        assert!(!resolve_issue_tracking(&enabled, false));
        assert!(resolve_issue_tracking(&enabled, true));
        assert!(!resolve_issue_tracking(&disabled, false));
        assert!(!resolve_issue_tracking(&disabled, true));
    }

    #[test]
    fn settings_json_uses_snake_case_enabled_field() {
        let value = serde_json::to_value(IssueTrackerSettings {
            enabled: Some(true),
        })
        .unwrap();

        assert_eq!(value, serde_json::json!({ "enabled": true }));
    }

    #[test]
    fn missing_settings_fall_back_to_build_allowance() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.json");
        let settings = load_issue_tracker_settings(&path);

        assert_eq!(settings, IssueTrackerSettings::default());
        assert!(!resolve_issue_tracking(&settings, false));
        assert!(resolve_issue_tracking(&settings, true));
    }

    #[test]
    fn missing_settings_read_error_is_not_a_warning() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.json");

        let error = load_issue_tracker_settings_result(&path).unwrap_err();

        assert!(!error.should_warn());
    }

    #[test]
    fn non_not_found_read_error_is_a_warning() {
        let directory = tempfile::tempdir().unwrap();
        let parent_file = directory.path().join("not-a-directory");
        std::fs::write(&parent_file, "not a directory").unwrap();
        let path = parent_file.join("settings.json");

        let error = load_issue_tracker_settings_result(&path).unwrap_err();

        assert!(error.should_warn());
    }

    #[test]
    fn malformed_settings_decode_error_is_a_warning() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid.json");
        std::fs::write(&path, "{ definitely not json").unwrap();

        let error = load_issue_tracker_settings_result(&path).unwrap_err();

        assert!(error.should_warn());
    }

    #[test]
    fn missing_or_null_enabled_field_deserializes_as_unset() {
        let directory = tempfile::tempdir().unwrap();
        let missing_key = directory.path().join("missing-key.json");
        let null_value = directory.path().join("null.json");
        std::fs::write(&missing_key, "{}").unwrap();
        std::fs::write(&null_value, r#"{"enabled":null}"#).unwrap();

        assert_eq!(
            load_issue_tracker_settings(&missing_key),
            IssueTrackerSettings::default()
        );
        assert_eq!(
            load_issue_tracker_settings(&null_value),
            IssueTrackerSettings::default()
        );
    }

    #[test]
    fn invalid_json_falls_back_to_build_allowance() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid.json");
        std::fs::write(&path, "{ definitely not json").unwrap();

        let settings = load_issue_tracker_settings(&path);
        assert_eq!(settings, IssueTrackerSettings::default());
        assert!(!resolve_issue_tracking(&settings, false));
        assert!(resolve_issue_tracking(&settings, true));
    }

    #[test]
    fn invalid_enabled_type_falls_back_to_build_allowance_in_both_directions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid-type.json");
        std::fs::write(&path, r#"{"enabled":"true"}"#).unwrap();

        let settings = load_issue_tracker_settings(&path);
        assert_eq!(settings, IssueTrackerSettings::default());
        assert!(!resolve_issue_tracking(&settings, false));
        assert!(resolve_issue_tracking(&settings, true));
    }

    #[test]
    fn save_and_load_round_trip_creates_parent_directory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("settings.json");

        for enabled in [Some(true), Some(false), None] {
            let expected = IssueTrackerSettings { enabled };
            save_issue_tracker_settings(&path, &expected).unwrap();

            assert_eq!(load_issue_tracker_settings(&path), expected);
        }

        assert!(path.parent().unwrap().is_dir());
    }

    #[test]
    fn effective_accessor_reads_settings_from_data_paths_with_build_allowance() {
        let directory = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(directory.path());

        save_issue_tracker_settings(
            &paths.issue_tracker_settings_path(),
            &IssueTrackerSettings {
                enabled: Some(true),
            },
        )
        .unwrap();

        assert!(!effective_issue_tracking_enabled_with_build_allows(
            &paths, false
        ));
        assert!(effective_issue_tracking_enabled_with_build_allows(
            &paths, true
        ));
    }

    #[test]
    fn response_preserves_explicit_value_and_reports_effective_value() {
        let directory = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(directory.path());
        save_issue_tracker_settings(
            &paths.issue_tracker_settings_path(),
            &IssueTrackerSettings {
                enabled: Some(false),
            },
        )
        .unwrap();

        let response = issue_tracker_settings_response(&paths, true);

        assert_eq!(response.enabled, Some(false));
        assert!(!response.effective_enabled);
    }

    #[test]
    fn response_preserves_saved_enabled_value_when_build_disallows_tracking() {
        let directory = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(directory.path());
        save_issue_tracker_settings(
            &paths.issue_tracker_settings_path(),
            &IssueTrackerSettings {
                enabled: Some(true),
            },
        )
        .unwrap();

        let response = issue_tracker_settings_response(&paths, false);

        assert_eq!(response.enabled, Some(true));
        assert!(!response.effective_enabled);
    }
}
