//! Commands for the independent Gateway work-eligibility preference.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::{
    error::AppResult,
    issue_work_policy::{WorkEligibility, WorkEligibilityRegistry},
};

use super::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct WorkEligibilityResponse {
    pub work_eligibility: WorkEligibility,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetWorkEligibilityRequest {
    pub work_eligibility: WorkEligibility,
}

#[tauri::command]
pub fn get_work_eligibility(state: State<'_, AppState>) -> AppResult<WorkEligibilityResponse> {
    Ok(WorkEligibilityResponse {
        work_eligibility: WorkEligibilityRegistry::open(&state.data.root).load(),
    })
}

#[tauri::command]
pub async fn set_work_eligibility(
    state: State<'_, AppState>,
    req: SetWorkEligibilityRequest,
) -> AppResult<WorkEligibilityResponse> {
    // The maintenance task uses the same lock before reading the persisted
    // value, so it cannot re-register the old policy during this update.
    let _gateway_registration_guard = state
        .sidecars
        .lock_issue_tracker_gateway_registration()
        .await;
    let registry = WorkEligibilityRegistry::open(&state.data.root);
    let current = registry.load();
    if current == req.work_eligibility {
        return Ok(WorkEligibilityResponse {
            work_eligibility: current,
        });
    }

    let service = state.issue_tracker_service().await?;
    let endpoint = state.issue_tracker_gateway_control_url().await?;
    let mode = req.work_eligibility.gateway_mode();
    let idempotency_key = format!(
        "lookback-work-eligibility-{}-{:?}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        mode
    );
    service
        .register_and_verify_gateway_policy(
            &endpoint,
            "lookback-work-eligibility-update",
            &idempotency_key,
            mode,
        )
        .await?;
    registry.update(req.work_eligibility)?;
    Ok(WorkEligibilityResponse {
        work_eligibility: req.work_eligibility,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_deserializes_the_two_supported_modes() {
        let request: SetWorkEligibilityRequest =
            serde_json::from_str(r#"{"work_eligibility":"INCLUDE_AUTO_REGISTERED"}"#).unwrap();
        assert_eq!(
            request.work_eligibility,
            WorkEligibility::IncludeAutoRegistered
        );
    }
}
