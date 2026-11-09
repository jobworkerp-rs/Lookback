//! Compile-time gates for functionality that is still being evaluated.

use serde::Serialize;

/// A release build can opt in explicitly while the normal developer profile
/// keeps experiments available without changing everyday commands.
pub const ISSUE_TRACKING_BUILD_ALLOWS: bool = issue_tracking_allowed_for_build(
    cfg!(debug_assertions),
    cfg!(feature = "experimental-issues"),
);

pub const fn issue_tracking_allowed_for_build(
    debug_assertions: bool,
    experimental_issues_feature: bool,
) -> bool {
    debug_assertions || experimental_issues_feature
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct AppCapabilities {
    pub experimental_issues_enabled: bool,
    pub issue_tracking_configurable: bool,
}

#[cfg(test)]
mod tests {
    use super::{AppCapabilities, issue_tracking_allowed_for_build};

    #[test]
    fn normal_release_disables_experimental_issues() {
        assert!(!issue_tracking_allowed_for_build(false, false));
    }

    #[test]
    fn debug_and_feature_enabled_builds_enable_experimental_issues() {
        assert!(issue_tracking_allowed_for_build(true, false));
        assert!(issue_tracking_allowed_for_build(false, true));
    }

    #[test]
    fn capabilities_include_issue_tracking_configuration_capability() {
        let capabilities = AppCapabilities {
            experimental_issues_enabled: true,
            issue_tracking_configurable: false,
        };

        assert_eq!(
            serde_json::to_value(capabilities).unwrap(),
            serde_json::json!({
                "experimental_issues_enabled": true,
                "issue_tracking_configurable": false,
            })
        );
    }
}
