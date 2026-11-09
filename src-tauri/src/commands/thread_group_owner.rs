//! Shared ThreadGroup owner request validation.

use crate::error::{AppError, AppResult};

/// Resolve the supported group-owner field and its legacy `user_id` alias.
///
/// The default belongs at the Rust command boundary so UI callers do not
/// silently manufacture an owner and workflows can require explicit input.
pub(crate) fn resolve_group_owner_user_id(
    group_owner_user_id: Option<i64>,
    legacy_user_id: Option<i64>,
) -> AppResult<i64> {
    if let Some(owner) = group_owner_user_id {
        validate_positive_user_id("group_owner_user_id", owner)?;
    }
    if let Some(owner) = legacy_user_id {
        validate_positive_user_id("user_id", owner)?;
    }

    if let (Some(owner), Some(legacy)) = (group_owner_user_id, legacy_user_id)
        && owner != legacy
    {
        return Err(AppError::Config(
            "group_owner_user_id and legacy user_id must match".into(),
        ));
    }

    Ok(group_owner_user_id.or(legacy_user_id).unwrap_or(1))
}

pub(crate) fn validate_positive_user_id(field: &str, user_id: i64) -> AppResult<i64> {
    if user_id <= 0 {
        return Err(AppError::Config(format!(
            "{field} must be a positive integer"
        )));
    }
    Ok(user_id)
}

pub(crate) fn validate_optional_user_id(
    field: &str,
    user_id: Option<i64>,
) -> AppResult<Option<i64>> {
    user_id
        .map(|user_id| validate_positive_user_id(field, user_id))
        .transpose()
}
