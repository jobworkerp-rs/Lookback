//! Background loop that periodically prunes old jobworkerp records
//! (`JobResult` / `JobProcessingStatus`).
//!
//! This intentionally avoids conductor/jobworkerp periodic jobs: cleanup
//! should not create user-visible execution history or extra job records.
//! It is app-lifecycle background work (spawned once at startup, like the
//! sidecars), not a Tauri command invoked from the frontend.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::data::DataPaths;
use crate::error::{AppError, AppResult};
use crate::jobworkerp::maintenance::{
    build_maintenance_requests, build_startup_orphan_sweep_request,
};
use crate::sidecar::Sidecars;

const RUN_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const READY_POLL_INTERVAL: Duration = Duration::from_secs(5);
const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct MaintenanceMarker {
    last_success_at_ms: i64,
}

pub fn spawn_jobworkerp_maintenance_loop(sidecars: Arc<Sidecars>, data: DataPaths) {
    tauri::async_runtime::spawn(async move {
        wait_for_sidecars(&sidecars).await;
        run_if_due(&sidecars, &data).await;

        let mut interval = tokio::time::interval(RUN_INTERVAL);
        loop {
            interval.tick().await;
            run_if_due(&sidecars, &data).await;
        }
    });
}

/// Schedule a best-effort sweep after every successful local sidecar start.
/// The caller supplies an endpoint published only after the full start
/// sequence, so the RPC cannot race the jobworkerp TCP health check.
pub(crate) fn spawn_startup_orphan_sweep(jobworkerp_url: String) {
    tauri::async_runtime::spawn(async move {
        let result = async {
            let handle = crate::jobworkerp::JobworkerpHandle::connect(&jobworkerp_url).await?;
            handle
                .purge_orphaned_job_processing_status(build_startup_orphan_sweep_request())
                .await
        }
        .await;
        let _ = finish_startup_orphan_sweep(result);
    });
}

fn completed_start_jobworkerp_url(
    report: Option<crate::sidecar::SidecarStartReport>,
) -> Option<String> {
    report.map(|report| report.endpoints.jobworkerp_url())
}

fn finish_startup_orphan_sweep(result: AppResult<u64>) -> Option<u64> {
    match result {
        Ok(marked_count) => {
            info!(
                marked_count,
                "jobworkerp startup orphaned status sweep completed"
            );
            Some(marked_count)
        }
        Err(e) => {
            warn!(error = %e, "jobworkerp startup orphaned status sweep failed");
            None
        }
    }
}

async fn wait_for_sidecars(sidecars: &Sidecars) {
    while completed_start_jobworkerp_url(sidecars.last_report()).is_none() {
        tokio::time::sleep(READY_POLL_INTERVAL).await;
    }
}

async fn run_if_due(sidecars: &Sidecars, data: &DataPaths) {
    let now_ms = current_time_ms();
    let marker_path = data.jobworkerp_maintenance_marker_path();
    if !should_run_maintenance(load_marker(&marker_path), now_ms) {
        debug!("jobworkerp maintenance skipped; last success is still recent");
        return;
    }
    let Some(url) = completed_start_jobworkerp_url(sidecars.last_report()) else {
        debug!("jobworkerp maintenance skipped; sidecars are not fully ready");
        return;
    };
    let result = async {
        let handle = crate::jobworkerp::JobworkerpHandle::connect(&url).await?;
        handle
            .run_maintenance(build_maintenance_requests(now_ms))
            .await
    }
    .await;
    match result {
        Ok(report) => {
            if let Err(e) = save_marker(&marker_path, now_ms) {
                warn!(error = %e, "jobworkerp maintenance completed but marker write failed");
                return;
            }
            info!(
                deleted_job_results = report.deleted_job_results,
                marked_stale_statuses = report.marked_stale_statuses,
                deleted_status_rows = report.deleted_status_rows,
                "jobworkerp maintenance completed"
            );
        }
        Err(e) => {
            warn!(error = %e, "jobworkerp maintenance failed");
        }
    }
}

fn current_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn should_run_maintenance(marker: Option<MaintenanceMarker>, now_ms: i64) -> bool {
    match marker {
        None => true,
        Some(marker) => {
            now_ms.saturating_sub(marker.last_success_at_ms) >= WEEK_MS
                || marker.last_success_at_ms > now_ms
        }
    }
}

fn load_marker(path: &Path) -> Option<MaintenanceMarker> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_marker(path: &Path, now_ms: i64) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let marker = MaintenanceMarker {
        last_success_at_ms: now_ms,
    };
    let bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|e| AppError::Config(format!("serialize maintenance marker: {e}")))?;
    std::fs::write(path, bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_run_without_marker() {
        assert!(should_run_maintenance(None, 1_700_000_000_000));
    }

    #[test]
    fn should_skip_before_one_week_elapsed() {
        let now = 1_700_000_000_000;
        let marker = MaintenanceMarker {
            last_success_at_ms: now - WEEK_MS + 1,
        };
        assert!(!should_run_maintenance(Some(marker), now));
    }

    #[test]
    fn should_run_after_one_week_elapsed() {
        let now = 1_700_000_000_000;
        let marker = MaintenanceMarker {
            last_success_at_ms: now - WEEK_MS,
        };
        assert!(should_run_maintenance(Some(marker), now));
    }

    #[test]
    fn should_run_when_marker_is_from_future() {
        let now = 1_700_000_000_000;
        let marker = MaintenanceMarker {
            last_success_at_ms: now + 1,
        };
        assert!(should_run_maintenance(Some(marker), now));
    }

    #[test]
    fn marker_roundtrip_saves_last_success_time() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("jobworkerp-maintenance.json");
        save_marker(&path, 1_700_000_000_000).unwrap();
        assert_eq!(
            load_marker(&path),
            Some(MaintenanceMarker {
                last_success_at_ms: 1_700_000_000_000
            })
        );
    }

    #[test]
    fn broken_marker_is_treated_as_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("jobworkerp-maintenance.json");
        std::fs::write(&path, b"{").unwrap();
        assert_eq!(load_marker(&path), None);
        assert!(should_run_maintenance(
            load_marker(&path),
            1_700_000_000_000
        ));
    }

    #[test]
    fn startup_orphan_sweep_failure_is_non_fatal() {
        assert_eq!(finish_startup_orphan_sweep(Ok(4)), Some(4));
        assert_eq!(
            finish_startup_orphan_sweep(Err(AppError::Jobworkerp("unavailable".into()))),
            None
        );
    }

    #[test]
    fn completed_start_jobworkerp_url_requires_a_successful_start_report() {
        assert_eq!(completed_start_jobworkerp_url(None), None);
        assert_eq!(
            completed_start_jobworkerp_url(Some(crate::sidecar::SidecarStartReport {
                endpoints: crate::sidecar::SidecarEndpoints {
                    jobworkerp_port: 19_000,
                    memories_port: 19_001,
                    conductor_port: 19_002,
                    mcp_server_port: None,
                },
                warnings: Vec::new(),
            })),
            Some("http://127.0.0.1:19000".to_string())
        );
    }
}
