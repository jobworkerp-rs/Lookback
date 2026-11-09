//! Persisted Gateway work-eligibility preference.
//!
//! This setting is deliberately separate from extraction policy: automatic
//! registration and whether workers may claim an unreviewed Issue are
//! independent operational decisions.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::fsutil::copy_atomic;
use crate::error::{AppError, AppResult};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkEligibility {
    #[default]
    ReviewedOnly,
    IncludeAutoRegistered,
}

impl WorkEligibility {
    pub fn gateway_mode(
        self,
    ) -> crate::grpc::proto::issue_tracker::manager::v1::WorkEligibilityMode {
        match self {
            Self::ReviewedOnly => {
                crate::grpc::proto::issue_tracker::manager::v1::WorkEligibilityMode::ReviewedOnly
            }
            Self::IncludeAutoRegistered => crate::grpc::proto::issue_tracker::manager::v1::WorkEligibilityMode::IncludeAutoRegistered,
        }
    }
}

pub struct WorkEligibilityRegistry {
    path: PathBuf,
}

impl WorkEligibilityRegistry {
    pub fn open(data_root: impl AsRef<Path>) -> Self {
        Self {
            path: data_root.as_ref().join("issue-work-eligibility.json"),
        }
    }

    pub fn load(&self) -> WorkEligibility {
        fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn update(&self, value: WorkEligibility) -> AppResult<()> {
        let parent = self.path.parent().ok_or_else(|| {
            AppError::Config("work-eligibility setting has no parent directory".into())
        })?;
        fs::create_dir_all(parent)?;
        // `copy_atomic` reserves `<destination>.partial`, so the source must
        // use a different sibling name rather than copying a file onto itself.
        let temporary = self.path.with_extension("json.write");
        let json = serde_json::to_vec_pretty(&value)
            .map_err(|error| AppError::Config(format!("serialize work eligibility: {error}")))?;
        fs::write(&temporary, json)?;
        copy_atomic(&temporary, &self.path)?;
        let _ = fs::remove_file(temporary);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::proto::issue_tracker::manager::v1::WorkEligibilityMode;

    #[test]
    fn work_eligibility_maps_to_the_gateway_policy_mode() {
        assert_eq!(
            WorkEligibility::ReviewedOnly.gateway_mode(),
            WorkEligibilityMode::ReviewedOnly
        );
        assert_eq!(
            WorkEligibility::IncludeAutoRegistered.gateway_mode(),
            WorkEligibilityMode::IncludeAutoRegistered
        );
    }

    #[test]
    fn missing_setting_defaults_to_reviewed_only() {
        let root = tempfile::tempdir().unwrap();

        assert_eq!(
            WorkEligibilityRegistry::open(root.path()).load(),
            WorkEligibility::ReviewedOnly
        );
    }

    #[test]
    fn update_persists_include_auto_registered() {
        let root = tempfile::tempdir().unwrap();
        let registry = WorkEligibilityRegistry::open(root.path());

        registry
            .update(WorkEligibility::IncludeAutoRegistered)
            .unwrap();

        assert_eq!(registry.load(), WorkEligibility::IncludeAutoRegistered);
    }
}
