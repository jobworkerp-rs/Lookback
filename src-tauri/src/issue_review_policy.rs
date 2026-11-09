//! Persisted requirements for human Issue review actions.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    data::fsutil::copy_atomic,
    error::{AppError, AppResult},
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueReviewPolicy {
    pub require_high_priority_reason: bool,
}

pub struct IssueReviewPolicyRegistry {
    path: PathBuf,
}

impl IssueReviewPolicyRegistry {
    pub fn open(data_root: impl AsRef<Path>) -> Self {
        Self {
            path: data_root.as_ref().join("issue-review-policy.json"),
        }
    }

    pub fn load(&self) -> IssueReviewPolicy {
        fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn update(&self, value: IssueReviewPolicy) -> AppResult<()> {
        let parent = self.path.parent().ok_or_else(|| {
            AppError::Config("issue review policy has no parent directory".into())
        })?;
        fs::create_dir_all(parent)?;
        let temporary = self.path.with_extension("json.write");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(&value).map_err(|error| {
                AppError::Config(format!("serialize issue review policy: {error}"))
            })?,
        )?;
        copy_atomic(&temporary, &self.path)?;
        let _ = fs::remove_file(temporary);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_policy_keeps_high_priority_reason_optional() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            IssueReviewPolicyRegistry::open(root.path()).load(),
            IssueReviewPolicy::default()
        );
    }

    #[test]
    fn policy_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let registry = IssueReviewPolicyRegistry::open(root.path());
        let policy = IssueReviewPolicy {
            require_high_priority_reason: true,
        };
        registry.update(policy.clone()).unwrap();
        assert_eq!(registry.load(), policy);
    }
}
