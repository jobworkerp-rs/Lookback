//! Retention policy for jobworkerp's own bookkeeping records (`JobResult` /
//! `JobProcessingStatus`). Kept separate from the thin RPC wrapper in
//! `jobworkerp/mod.rs` since this module owns a business decision (what
//! counts as "stale"), not just request/response plumbing.

use jobworkerp_client::jobworkerp::service::{
    CleanupRequest, DeleteJobResultBulkRequest, PurgeStaleJobsRequest,
};

/// How long to keep `JobResult` / `JobProcessingStatus` rows before they're
/// eligible for cleanup.
const RETENTION_DAYS: i64 = 7;
const RETENTION_HOURS: u64 = (RETENTION_DAYS * 24) as u64;
const RETENTION_MS: i64 = RETENTION_DAYS * 24 * 60 * 60 * 1000;
const STARTUP_ORPHAN_SWEEP_THRESHOLD_HOURS: u64 = 0;

#[derive(Debug, Clone, PartialEq)]
pub struct MaintenanceRequests {
    pub delete_bulk: DeleteJobResultBulkRequest,
    pub purge_stale_jobs: PurgeStaleJobsRequest,
    pub cleanup: CleanupRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub deleted_job_results: i64,
    pub marked_stale_statuses: u64,
    pub deleted_status_rows: u64,
}

pub fn build_maintenance_requests(now_ms: i64) -> MaintenanceRequests {
    MaintenanceRequests {
        delete_bulk: DeleteJobResultBulkRequest {
            end_time_before: Some(now_ms.saturating_sub(RETENTION_MS)),
            statuses: Vec::new(),
            worker_ids: Vec::new(),
        },
        purge_stale_jobs: PurgeStaleJobsRequest {
            stale_threshold_hours: RETENTION_HOURS,
            orphaned_only: Some(true),
        },
        cleanup: CleanupRequest {
            retention_hours_override: Some(RETENTION_HOURS),
        },
    }
}

/// Build the startup-only sweep request. Zero selects every status row older
/// than the current instant; orphaned-only verification still protects rows
/// that have a live status or a persisted job.
pub fn build_startup_orphan_sweep_request() -> PurgeStaleJobsRequest {
    PurgeStaleJobsRequest {
        stale_threshold_hours: STARTUP_ORPHAN_SWEEP_THRESHOLD_HOURS,
        orphaned_only: Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_requests_use_one_week_retention_and_orphaned_purge() {
        let now_ms = 1_700_000_000_000;
        let requests = build_maintenance_requests(now_ms);

        assert_eq!(
            requests.delete_bulk.end_time_before,
            Some(now_ms - 7 * 24 * 60 * 60 * 1000)
        );
        assert_eq!(requests.delete_bulk.statuses, Vec::<i32>::new());
        assert!(requests.delete_bulk.worker_ids.is_empty());
        assert_eq!(requests.purge_stale_jobs.stale_threshold_hours, 168);
        assert_eq!(requests.purge_stale_jobs.orphaned_only, Some(true));
        assert_eq!(requests.cleanup.retention_hours_override, Some(168));
    }

    #[test]
    fn startup_orphan_sweep_checks_every_orphaned_status_row() {
        let request = build_startup_orphan_sweep_request();

        assert_eq!(request.stale_threshold_hours, 0);
        assert_eq!(request.orphaned_only, Some(true));
    }
}
