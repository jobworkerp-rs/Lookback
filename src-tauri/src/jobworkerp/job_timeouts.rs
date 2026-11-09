//! Server-side job deadlines for the WORKFLOW workers the app dispatches.
//!
//! Each deadline is the worker's sized run time (also used by periodic-run
//! calls to the same batch, see `workers/README.md`, "タイムアウト") plus a grace
//! period. The same value is used as the client request deadline: the default
//! three-hour client would otherwise close a longer result stream early. Work
//! inside each batch is bounded by the one-hour run/call default the app
//! configures for jobworkerp.

pub(crate) const MINUTE: u64 = 60;
pub(crate) const HOUR: u64 = 60 * MINUTE;
const GRACE: u64 = HOUR;

/// `memories-summarize-batch` (thread-summary-batch.yaml).
pub const THREAD_SUMMARY_BATCH: u64 = 115 * HOUR + GRACE;
/// `memories-thread-group-summary-batch` (thread-group-summary-batch.yaml).
pub const THREAD_GROUP_SUMMARY_BATCH: u64 = 121 * HOUR + GRACE;
/// `memories-daily-summary-batch` (daily-work-summary-batch.yaml).
pub const DAILY_SUMMARY_BATCH: u64 = 11 * HOUR + GRACE;
/// `memories-weekly-summary-batch` (weekly-work-summary-batch.yaml).
pub const WEEKLY_SUMMARY_BATCH: u64 = 11 * HOUR + GRACE;
/// `memories-monthly-summary-batch` (monthly-work-summary-batch.yaml).
pub const MONTHLY_SUMMARY_BATCH: u64 = 7 * HOUR + GRACE;
/// `memories-personality-batch` (thread-personality-batch.yaml).
pub const PERSONALITY_BATCH: u64 = 19 * HOUR + GRACE;
/// `memories-user-personality-merge-<lang>` (user-personality-merge.yaml).
pub const PERSONALITY_MERGE: u64 = HOUR + GRACE;
/// `memories-reflection-batch` (thread-reflection-batch.yaml).
pub const REFLECTION_BATCH: u64 = 27 * HOUR + GRACE;
/// `lookback-thread-issue-resolution-manual` (thread-issue-resolution-manual.yaml).
pub const THREAD_ISSUE_RESOLUTION_MANUAL: u64 = 24 * HOUR + GRACE;
/// `lookback-thread-group-issue-resolution` (thread-group-issue-resolution-batch.yaml).
pub const THREAD_GROUP_ISSUE_RESOLUTION: u64 = 24 * HOUR + GRACE;

/// Client request deadline for import and the summaries tab, which stream
/// several staged batches over one connection: the longest staged batch, so
/// no stage's result stream is closed early.
pub const STAGED_GENERATION_CLIENT: u64 = max_of(&[
    THREAD_SUMMARY_BATCH,
    THREAD_GROUP_SUMMARY_BATCH,
    PERSONALITY_BATCH,
    REFLECTION_BATCH,
    DAILY_SUMMARY_BATCH,
    WEEKLY_SUMMARY_BATCH,
    MONTHLY_SUMMARY_BATCH,
]);

/// Conductor's job timeout for `memories-lookback-periodic-run`, which waits
/// for the same staged batches. As a streaming job's timeout it bounds the
/// silence between outputs, and a stage reports only when it ends.
pub const PERIODIC_RUN: u64 = STAGED_GENERATION_CLIENT;

const fn max_of(values: &[u64]) -> u64 {
    let mut max = 0;
    let mut i = 0;
    while i < values.len() {
        if values[i] > max {
            max = values[i];
        }
        i += 1;
    }
    max
}
