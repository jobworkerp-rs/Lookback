//! IPC boundary for Lookback-owned LanceDB index maintenance.

use serde::Deserialize;
use tauri::State;

use crate::commands::AppState;
use crate::commands::connection::ConnectionMode;
use crate::error::{AppError, AppResult};
use crate::search_index_maintenance::{MaintenanceSchedule, MaintenanceStatus};

fn require_local(state: &AppState) -> AppResult<()> {
    if state.connection_mode() != ConnectionMode::Local {
        return Err(AppError::Config(
            "リモート接続ではローカル index メンテナンスを実行できません".into(),
        ));
    }
    Ok(())
}

#[tauri::command]
pub async fn get_search_index_maintenance_status(
    state: State<'_, AppState>,
) -> AppResult<MaintenanceStatus> {
    require_local(&state)?;
    Ok(state.search_index_maintenance.status().await)
}

/// Starts only the explicit request. Completion is obtained from the status
/// polling path; accepting an RPC response is never presented as completion.
#[tauri::command]
pub async fn start_search_index_maintenance(state: State<'_, AppState>) -> AppResult<()> {
    require_local(&state)?;
    let channel = state.memories_channel().await?;
    let coordinator = state.search_index_maintenance.clone();
    let accepted = coordinator.begin_manual_optimize(channel.clone()).await?;
    if let Some(accepted) = accepted {
        tauri::async_runtime::spawn(coordinator.observe_accepted_optimize(channel, accepted));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSearchIndexMaintenanceScheduleRequest {
    pub enabled: bool,
    pub start: Option<String>,
    pub end: Option<String>,
}

/// Scheduling fields are intentionally accepted only after validation.  The
/// persistent scheduler is introduced behind this stable IPC shape.
#[tauri::command]
pub async fn set_search_index_maintenance_schedule(
    state: State<'_, AppState>,
    req: SetSearchIndexMaintenanceScheduleRequest,
) -> AppResult<()> {
    require_local(&state)?;
    if req.enabled {
        let (Some(start), Some(end)) = (req.start.as_deref(), req.end.as_deref()) else {
            return Err(AppError::Config(
                "保守時間帯の開始・終了時刻が必要です".into(),
            ));
        };
        if start == end || !is_time_of_day(start) || !is_time_of_day(end) {
            return Err(AppError::Config("保守時間帯の時刻が不正です".into()));
        }
    }
    state
        .search_index_maintenance
        .set_schedule(MaintenanceSchedule {
            enabled: req.enabled,
            start: req.start,
            end: req.end,
        })
        .await
}

fn is_time_of_day(value: &str) -> bool {
    let Some((hour, minute)) = value.split_once(':') else {
        return false;
    };
    hour.len() == 2
        && minute.len() == 2
        && hour.parse::<u8>().is_ok_and(|hour| hour < 24)
        && minute.parse::<u8>().is_ok_and(|minute| minute < 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_window_requires_distinct_valid_times() {
        assert!(is_time_of_day("00:00"));
        assert!(is_time_of_day("23:59"));
        assert!(!is_time_of_day("24:00"));
        assert!(!is_time_of_day("9:00"));
    }
}
