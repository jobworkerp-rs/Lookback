//! Live jobworkerp queue counts for the Settings observability card.

use futures::future::try_join_all;
use jobworkerp_client::jobworkerp::data::JobProcessingStatus;
use jobworkerp_client::jobworkerp::service::{
    CountJobProcessingStatusMode, CountJobProcessingStatusRequest, CountJobProcessingStatusResponse,
};
use serde::Serialize;
use tauri::State;

use crate::error::{AppError, AppResult};
use crate::jobworkerp::JobworkerpHandle;
use crate::sidecar::SidecarEndpoints;

use super::AppState;
use super::analysis_dispatch::{
    PERSONALITY_MERGE_WORKER_BASE, PERSONALITY_WORKER_NAME, SUMMARIES_PIPELINE_WORKER_NAME,
    SUMMARY_WORKER_NAME,
};
use super::import::PeriodKind;
use super::reflection_dispatch::REFLECTION_WORKER_NAME;

const EMBEDDING_CHANNELS: &[&str] = &["embedding", "embedding_workflow"];
const PERIOD_KINDS: [PeriodKind; 3] = [PeriodKind::Daily, PeriodKind::Weekly, PeriodKind::Monthly];
const SUMMARY_SINGLE_WORKER_BASES: &[&str] = &[
    "memories-thread-summary-single",
    "memories-daily-work-summary-single",
    "memories-weekly-work-summary-single",
    "memories-monthly-work-summary-single",
];
const PERSONALITY_SINGLE_WORKER_BASE: &str = "memories-thread-personality-single";
const REFLECTION_SINGLE_WORKER_BASE: &str = "memories-thread-reflection-single";

fn local_jobworkerp_url(endpoints: Option<SidecarEndpoints>) -> AppResult<String> {
    endpoints
        .map(|endpoints| endpoints.jobworkerp_url())
        .ok_or_else(|| AppError::Config("local jobworkerp sidecar is not running".into()))
}

fn extend_language_worker_names(names: &mut Vec<String>, base: &str) {
    names.extend(
        super::SUPPORTED_LANGUAGES
            .iter()
            .map(|language| format!("{base}-{language}")),
    );
}

fn summary_worker_names() -> Vec<String> {
    let mut names = vec![
        SUMMARY_WORKER_NAME.to_string(),
        SUMMARIES_PIPELINE_WORKER_NAME.to_string(),
    ];
    names.extend(
        PERIOD_KINDS
            .iter()
            .map(|kind| kind.worker_name().to_string()),
    );
    for base in SUMMARY_SINGLE_WORKER_BASES {
        extend_language_worker_names(&mut names, base);
    }
    names
}

fn personality_worker_names() -> Vec<String> {
    let mut names = vec![PERSONALITY_WORKER_NAME.to_string()];
    extend_language_worker_names(&mut names, PERSONALITY_MERGE_WORKER_BASE);
    extend_language_worker_names(&mut names, PERSONALITY_SINGLE_WORKER_BASE);
    names
}

fn reflection_worker_names() -> Vec<String> {
    let mut names = vec![REFLECTION_WORKER_NAME.to_string()];
    extend_language_worker_names(&mut names, REFLECTION_SINGLE_WORKER_BASE);
    names
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskKind {
    Embedding,
    Summary,
    Personality,
    Reflection,
    LlmOther,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BackgroundJobCounts {
    pub pending: u64,
    pub running: u64,
    pub wait_result: u64,
    pub cancelling: u64,
}

impl BackgroundJobCounts {
    fn active(&self) -> bool {
        self.pending + self.running + self.wait_result + self.cancelling > 0
    }

    fn add_assign(&mut self, other: &Self) {
        self.pending += other.pending;
        self.running += other.running;
        self.wait_result += other.wait_result;
        self.cancelling += other.cancelling;
    }

    fn saturating_sub(&self, other: &Self) -> Self {
        Self {
            pending: self.pending.saturating_sub(other.pending),
            running: self.running.saturating_sub(other.running),
            wait_result: self.wait_result.saturating_sub(other.wait_result),
            cancelling: self.cancelling.saturating_sub(other.cancelling),
        }
    }
}

impl std::iter::Sum for BackgroundJobCounts {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), |mut total, counts| {
            total.add_assign(&counts);
            total
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundJobQueueRow {
    pub kind: BackgroundTaskKind,
    #[serde(flatten)]
    pub counts: BackgroundJobCounts,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundJobQueueStatus {
    pub rows: Vec<BackgroundJobQueueRow>,
    pub active: bool,
}

fn counts_from_response(response: CountJobProcessingStatusResponse) -> BackgroundJobCounts {
    let mut counts = BackgroundJobCounts::default();
    for entry in response.counts {
        match JobProcessingStatus::try_from(entry.status).ok() {
            Some(JobProcessingStatus::Pending) => counts.pending += entry.count.max(0) as u64,
            Some(JobProcessingStatus::Running) => counts.running += entry.count.max(0) as u64,
            Some(JobProcessingStatus::WaitResult) => {
                counts.wait_result += entry.count.max(0) as u64
            }
            Some(JobProcessingStatus::Cancelling) => counts.cancelling += entry.count.max(0) as u64,
            _ => {}
        }
    }
    counts
}

async fn count_by_condition(
    handle: &JobworkerpHandle,
    worker_id: Option<i64>,
    worker_channel: Option<&str>,
) -> AppResult<BackgroundJobCounts> {
    let response = handle
        .count_job_processing_status(CountJobProcessingStatusRequest {
            status: None,
            worker_id,
            channel: worker_channel.map(str::to_owned),
            min_elapsed_time_ms: None,
            mode: CountJobProcessingStatusMode::GroupByStatus as i32,
        })
        .await?;
    Ok(counts_from_response(response))
}

async fn worker_ids<T: AsRef<str>>(handle: &JobworkerpHandle, names: &[T]) -> AppResult<Vec<i64>> {
    let futures = names
        .iter()
        .map(|name| async move { handle.worker_id_by_name(name.as_ref()).await });
    Ok(try_join_all(futures).await?.into_iter().flatten().collect())
}

async fn count_workers(handle: &JobworkerpHandle, ids: Vec<i64>) -> AppResult<BackgroundJobCounts> {
    let results = try_join_all(
        ids.into_iter()
            .map(|id| count_by_condition(handle, Some(id), None)),
    )
    .await?;
    Ok(results.into_iter().sum())
}

#[tauri::command]
pub async fn get_background_job_queue_status(
    state: State<'_, AppState>,
) -> AppResult<BackgroundJobQueueStatus> {
    // This is an observability card for the always-local sidecar. Do not use
    // AppState::jobworkerp(): Remote browse mode intentionally redirects that
    // shared client to the configured remote service.
    let local_url = local_jobworkerp_url(state.sidecars.current_endpoints())?;
    let handle = JobworkerpHandle::connect(&local_url).await?;

    let summary_names = summary_worker_names();
    let personality_names = personality_worker_names();
    let reflection_names = reflection_worker_names();
    let (summary_ids, personality_ids, reflection_ids) = tokio::try_join!(
        worker_ids(&handle, &summary_names),
        worker_ids(&handle, &personality_names),
        worker_ids(&handle, &reflection_names),
    )?;
    let (embedding_counts, summary_counts, personality_counts, reflection_counts, total_counts) = tokio::try_join!(
        async {
            let counts = try_join_all(
                EMBEDDING_CHANNELS
                    .iter()
                    .map(|channel| count_by_condition(&handle, None, Some(channel))),
            )
            .await?;
            Ok::<_, AppError>(counts.into_iter().sum())
        },
        count_workers(&handle, summary_ids),
        count_workers(&handle, personality_ids),
        count_workers(&handle, reflection_ids),
        count_by_condition(&handle, None, None),
    )?;

    let classified: BackgroundJobCounts = [
        &embedding_counts,
        &summary_counts,
        &personality_counts,
        &reflection_counts,
    ]
    .into_iter()
    .cloned()
    .sum();
    let other_counts = total_counts.saturating_sub(&classified);
    let rows = vec![
        BackgroundJobQueueRow {
            kind: BackgroundTaskKind::Embedding,
            counts: embedding_counts,
        },
        BackgroundJobQueueRow {
            kind: BackgroundTaskKind::Summary,
            counts: summary_counts,
        },
        BackgroundJobQueueRow {
            kind: BackgroundTaskKind::Personality,
            counts: personality_counts,
        },
        BackgroundJobQueueRow {
            kind: BackgroundTaskKind::Reflection,
            counts: reflection_counts,
        },
        BackgroundJobQueueRow {
            kind: BackgroundTaskKind::LlmOther,
            counts: other_counts,
        },
    ];
    Ok(BackgroundJobQueueStatus {
        active: rows.iter().any(|row| row.counts.active()),
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_from_response_maps_active_statuses_and_ignores_unknown() {
        use jobworkerp_client::jobworkerp::service::JobProcessingStatusCount;

        let counts = counts_from_response(CountJobProcessingStatusResponse {
            total: 10,
            counts: vec![
                JobProcessingStatusCount {
                    status: JobProcessingStatus::Pending as i32,
                    count: 2,
                },
                JobProcessingStatusCount {
                    status: JobProcessingStatus::Running as i32,
                    count: 3,
                },
                JobProcessingStatusCount {
                    status: JobProcessingStatus::WaitResult as i32,
                    count: 1,
                },
                JobProcessingStatusCount {
                    status: JobProcessingStatus::Cancelling as i32,
                    count: 1,
                },
                JobProcessingStatusCount {
                    status: JobProcessingStatus::Unknown as i32,
                    count: 99,
                },
            ],
            mode: CountJobProcessingStatusMode::GroupByStatus as i32,
        });
        assert_eq!(
            counts,
            BackgroundJobCounts {
                pending: 2,
                running: 3,
                wait_result: 1,
                cancelling: 1
            }
        );
    }

    #[test]
    fn count_subtraction_never_underflows() {
        let remaining = BackgroundJobCounts {
            pending: 1,
            running: 0,
            wait_result: 2,
            cancelling: 0,
        }
        .saturating_sub(&BackgroundJobCounts {
            pending: 3,
            running: 1,
            wait_result: 1,
            cancelling: 1,
        });
        assert_eq!(
            remaining,
            BackgroundJobCounts {
                pending: 0,
                running: 0,
                wait_result: 1,
                cancelling: 0
            }
        );
    }

    #[test]
    fn summary_worker_names_cover_batch_and_period_pipeline() {
        let names = summary_worker_names();
        assert!(names.contains(&"memories-summarize-batch".to_string()));
        assert!(names.contains(&"memories-summaries-pipeline".to_string()));
        assert!(names.contains(&"memories-daily-summary-batch".to_string()));
        assert!(names.contains(&"memories-weekly-summary-batch".to_string()));
        assert!(names.contains(&"memories-monthly-summary-batch".to_string()));
        for language in super::super::SUPPORTED_LANGUAGES {
            assert!(names.contains(&format!("memories-thread-summary-single-{language}")));
            assert!(names.contains(&format!("memories-daily-work-summary-single-{language}")));
            assert!(names.contains(&format!("memories-weekly-work-summary-single-{language}")));
            assert!(names.contains(&format!("memories-monthly-work-summary-single-{language}")));
        }
    }

    #[test]
    fn personality_worker_names_include_language_specific_merge_workers() {
        let names = personality_worker_names();
        assert!(names.contains(&PERSONALITY_WORKER_NAME.to_string()));
        for language in super::super::SUPPORTED_LANGUAGES {
            assert!(names.contains(&format!("memories-user-personality-merge-{language}")));
            assert!(names.contains(&format!("memories-thread-personality-single-{language}")));
        }
    }

    #[test]
    fn reflection_worker_names_include_language_specific_single_workers() {
        let names = reflection_worker_names();
        assert!(names.contains(&REFLECTION_WORKER_NAME.to_string()));
        for language in super::super::SUPPORTED_LANGUAGES {
            assert!(names.contains(&format!("memories-thread-reflection-single-{language}")));
        }
    }

    #[test]
    fn local_jobworkerp_url_uses_live_sidecar_endpoint() {
        let endpoints = crate::sidecar::SidecarEndpoints {
            jobworkerp_port: 19_000,
            memories_port: 19_001,
            conductor_port: 19_002,
            mcp_server_port: None,
        };
        assert_eq!(
            local_jobworkerp_url(Some(endpoints)).unwrap(),
            "http://127.0.0.1:19000"
        );
    }
}
