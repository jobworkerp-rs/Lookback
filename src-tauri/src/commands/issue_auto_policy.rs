//! Commands for the persisted extraction and automatic registration policy.

use std::path::Path;

use serde::Deserialize;
use tauri::State;

use crate::{
    error::AppResult,
    issue_extract::{AutoIssuePolicy, PolicyRegistry, PolicySnapshot},
};

use super::AppState;

#[derive(Debug, Clone, Deserialize)]
pub struct SetAutoIssuePolicyRequest {
    pub policy: AutoIssuePolicy,
}

pub fn update_auto_issue_policy(
    data_root: impl AsRef<Path>,
    request: SetAutoIssuePolicyRequest,
) -> AppResult<PolicySnapshot> {
    PolicyRegistry::open(data_root).update(request.policy)
}

#[tauri::command]
pub fn get_auto_issue_policy(state: State<'_, AppState>) -> AppResult<PolicySnapshot> {
    Ok(PolicyRegistry::open(&state.data.root).load())
}

#[tauri::command]
pub fn set_auto_issue_policy(
    state: State<'_, AppState>,
    req: SetAutoIssuePolicyRequest,
) -> AppResult<PolicySnapshot> {
    update_auto_issue_policy(&state.data.root, req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue_extract::{
        AutoIssuePolicy, AutoRegisterPriority, AutoRegisterPriorityRange, PolicyRegistry,
    };
    use tempfile::tempdir;

    #[test]
    fn request_persists_a_valid_policy_and_returns_the_effective_snapshot() {
        let root = tempdir().unwrap();
        let request = SetAutoIssuePolicyRequest {
            policy: AutoIssuePolicy {
                extraction_enabled: true,
                auto_register_min_confidence: Some(0.95),
                auto_register_priority_range: AutoRegisterPriorityRange {
                    lower: AutoRegisterPriority::P0,
                    upper: AutoRegisterPriority::P1,
                },
                explicit_only: false,
                allowed_kinds: vec!["TASK".into()],
                extract_cap: 10,
                ..AutoIssuePolicy::default()
            },
        };

        let saved = update_auto_issue_policy(root.path(), request).unwrap();

        assert_eq!(saved.version, 2);
        assert_eq!(saved.policy.auto_register_min_confidence, Some(0.95));
        assert_eq!(saved.policy.resolution_min_confidence, 0.90);
        assert_eq!(
            PolicyRegistry::open(root.path()).load().policy,
            saved.policy
        );
    }

    #[test]
    fn request_rejects_invalid_policy_boundaries() {
        let root = tempdir().unwrap();
        let request = SetAutoIssuePolicyRequest {
            policy: AutoIssuePolicy {
                auto_register_min_confidence: Some(1.01),
                ..AutoIssuePolicy::default()
            },
        };

        assert!(update_auto_issue_policy(root.path(), request).is_err());
    }

    #[test]
    fn request_rejects_invalid_resolution_confidence() {
        let root = tempdir().unwrap();
        let request = SetAutoIssuePolicyRequest {
            policy: AutoIssuePolicy {
                resolution_min_confidence: 1.01,
                ..AutoIssuePolicy::default()
            },
        };

        assert!(update_auto_issue_policy(root.path(), request).is_err());
    }

    #[test]
    fn request_reports_policy_storage_failures() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("issue-auto-policy"), "not a directory").unwrap();

        let error = update_auto_issue_policy(
            root.path(),
            SetAutoIssuePolicyRequest {
                policy: AutoIssuePolicy::default(),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("directory") || error.to_string().contains("File"));
    }
}
