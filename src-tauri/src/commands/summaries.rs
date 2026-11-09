//! Tauri commands backing the Summaries page.
//!
//! Summaries are lifecycle-kind memories owned by the Lookback user. Their
//! `external_id` encodes the summary kind:
//!
//! - per-thread: `summary:<thread_id>`
//! - thread-group: `thread-group-summary:<group_id>`
//! - daily:      `daily:<user_id>:<YYYY-MM-DD>:<scope_key>`
//! - weekly:     `weekly:<user_id>:<YYYY-Www>:<scope_key>`  (ISO 8601 week)
//! - monthly:    `monthly:<user_id>:<YYYY-MM>:<scope_key>`
//!
//! The `content` field is a JSON document with `title` / `context` /
//! `decisions` / `open_questions` / `followups` categories (see
//! `lang-workers/workers/thread-summary/thread-summary-single.yaml` and the
//! `*-work-summary` workflows).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use tauri::State;
use tokio_stream::StreamExt;

use crate::error::AppResult;
use crate::grpc::proto::llm_memory::data as mem_data;
use crate::grpc::proto::llm_memory::service as mem_svc;
use crate::grpc::proto::llm_memory::service::memory_service_client::MemoryServiceClient;
use crate::grpc::proto::llm_memory::service::thread_service_client::ThreadServiceClient;

use super::AppState;
use super::threads::{LabelMatch, LabelWithCount};

/// Synthetic owner of every summary memory (per-thread and period). Shared
/// with the dispatch input builders in `commands::import`.
pub(crate) const LOOKBACK_USER_ID: i64 = 1;

/// The summary granularities, distinguished by `external_id` prefix. Each
/// period kind also carries a summary-thread label (`<kind>_summary`) that
/// the frontend uses to scope full-text search to a granularity — search
/// filters cannot match on `external_id`, only on thread labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SummaryKind {
    /// One summary per imported conversation thread (`summary:<thread_id>`).
    PerThread,
    /// One summary for the current membership snapshot of a ThreadGroup.
    #[default]
    ThreadGroup,
    Daily,
    Weekly,
    Monthly,
}

impl SummaryKind {
    /// Stable namespace used when parsing and server-side filtering. Queries
    /// combine this prefix with `user_id` and `memory_kind`: the kind guards
    /// the typed row while the prefix prevents stale namespaces from crossing
    /// tabs before pagination is applied.
    fn external_id_namespace(self) -> &'static str {
        match self {
            SummaryKind::PerThread => "summary:",
            SummaryKind::ThreadGroup => "thread-group-summary:",
            SummaryKind::Daily => "daily:",
            SummaryKind::Weekly => "weekly:",
            SummaryKind::Monthly => "monthly:",
        }
    }
}

/// The aggregation level of a v2 period summary. Per-thread and legacy
/// period summaries deliberately have no value because the old identifier
/// cannot prove this distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollupScope {
    Overall,
    Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionStatus {
    Complete,
    Incomplete,
}

/// A Path identity safe to return only after the parent summary owner has
/// been verified by the list query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryPathBucket {
    pub path_bucket_digest: String,
    pub path_bucket_kind: String,
    pub path_label: Option<String>,
}

/// A persisted explanation for a path child that was unavailable or reused
/// while an overall rollup was still produced best-effort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryPathBucketDiagnostic {
    pub path_bucket_digest: String,
    pub statuses: Vec<String>,
    pub path_bucket_kind: String,
    pub path_label: Option<String>,
}

/// Canonical Path classification used by the generation planner. The stored
/// `path:` label is opaque here: import owns normalization, while summaries
/// bucket the already-persisted value without changing it.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathBucket {
    Label(String),
    Unassigned,
    Ambiguous,
}

#[cfg(test)]
impl PathBucket {
    pub(crate) fn from_thread_labels(labels: &[String]) -> Self {
        let mut paths: Vec<&String> = labels
            .iter()
            .filter(|label| label.starts_with("path:"))
            .collect();
        paths.sort();
        paths.dedup();
        match paths.as_slice() {
            [] => Self::Unassigned,
            [path] => Self::Label((*path).clone()),
            _ => Self::Ambiguous,
        }
    }

    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Label(_) => "label",
            Self::Unassigned => "unassigned",
            Self::Ambiguous => "ambiguous",
        }
    }

    pub(crate) fn path_label(&self) -> Option<&str> {
        match self {
            Self::Label(label) => Some(label),
            Self::Unassigned | Self::Ambiguous => None,
        }
    }

    pub(crate) fn digest(&self) -> String {
        let value = match self {
            Self::Label(label) => serde_json::json!({
                "kind": "label",
                "label": label,
                "version": 1,
            }),
            Self::Unassigned => serde_json::json!({ "kind": "unassigned", "version": 1 }),
            Self::Ambiguous => serde_json::json!({ "kind": "ambiguous", "version": 1 }),
        };
        sha256_hex(&serde_json::to_string(&value).expect("canonical path JSON serializes"))
    }
}

/// Source-scope identity includes its declared matching mode even with fewer
/// than two labels. The JSON key order is the v2 storage contract.
pub(crate) fn source_scope_digest(labels: &[String], match_mode: Option<&str>) -> String {
    let mut labels = labels.to_vec();
    labels.sort();
    labels.dedup();
    let value = serde_json::json!({
        "labels": labels,
        "match_mode": match_mode.unwrap_or("LABEL_ALL"),
        "version": 1,
    });
    sha256_hex(&serde_json::to_string(&value).expect("canonical source scope JSON serializes"))
}

/// Compute the v2 path-bucket identity used in worker external IDs and
/// recovery records. Keep this in Rust so callers never need to reproduce the
/// worker's canonical JSON or depend on a shell hashing utility.
pub(crate) fn path_bucket_digest(kind: &str, path_label: Option<&str>) -> Option<String> {
    let value = match kind {
        "label" => serde_json::json!({
            "kind": "label",
            "label": path_label.filter(|label| !label.is_empty())?,
            "version": 1,
        }),
        "unassigned" | "ambiguous" => serde_json::json!({
            "kind": kind,
            "version": 1,
        }),
        _ => return None,
    };
    Some(sha256_hex(
        &serde_json::to_string(&value).expect("canonical path JSON serializes"),
    ))
}

/// Preserve a previously readable overall body after a later generation
/// attempt fails, while making that failed attempt visible on the Memory
/// itself.  This deliberately changes metadata only: the material generation
/// ID, content, and source fingerprint remain those of the last successful
/// body.  A caller still records durable recovery state if this write fails.
pub(crate) async fn mark_failed_overall_attempts(
    state: &AppState,
    kind: SummaryKind,
    source_scope_digest: &str,
    generation_id: &str,
    generation_started_at: &str,
    period_keys: &[String],
    reason_code: &str,
) -> AppResult<usize> {
    if period_keys.is_empty() || source_scope_digest.is_empty() || generation_id.is_empty() {
        return Ok(0);
    }
    let wanted: HashSet<&str> = period_keys.iter().map(String::as_str).collect();
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);
    let mut stream = client
        .find_list_by_condition(build_summary_memory_request_for(kind))
        .await?
        .into_inner();
    let mut targets = Vec::new();
    while let Some(item) = stream.next().await {
        let Some(memory) = item?.memory else {
            continue;
        };
        let Some(external_id) = memory
            .data
            .as_ref()
            .and_then(|data| data.external_id.as_deref())
        else {
            continue;
        };
        if wanted.iter().any(|period_key| {
            external_id
                == format!(
                    "{}v2:{LOOKBACK_USER_ID}:{period_key}:{source_scope_digest}:overall",
                    kind.external_id_namespace()
                )
        }) {
            targets.push(memory);
        }
    }
    drop(stream);

    let mut updated = 0;
    for mut memory in targets {
        let Some(data) = memory.data.as_mut() else {
            continue;
        };
        let Ok(mut metadata) =
            serde_json::from_str::<serde_json::Value>(data.metadata.as_deref().unwrap_or(""))
        else {
            continue;
        };
        let Some(metadata) = metadata.as_object_mut() else {
            continue;
        };
        metadata.insert(
            "last_material_attempt_generation_id".to_string(),
            serde_json::Value::String(generation_id.to_string()),
        );
        metadata.insert(
            "last_material_attempt_started_at".to_string(),
            serde_json::Value::String(generation_started_at.to_string()),
        );
        metadata.insert(
            "last_material_attempt_status".to_string(),
            serde_json::Value::String("failed".to_string()),
        );
        metadata.insert(
            "last_material_attempt_error_codes".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String(reason_code.to_string())]),
        );
        data.metadata = Some(serde_json::Value::Object(metadata.clone()).to_string());
        client.update(memory).await?;
        updated += 1;
    }
    Ok(updated)
}

fn sha256_hex(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[derive(Debug, Deserialize)]
struct V2SummaryMetadata {
    aggregation_mode: String,
    completion_status: CompletionStatus,
    generation_id: String,
    generation_started_at: String,
    generation_consistency: String,
    #[serde(default)]
    input_generation_ids: Vec<String>,
    #[serde(default)]
    input_without_generation_id_count: usize,
    #[serde(default)]
    failed_path_bucket_digests: Vec<String>,
    #[serde(default)]
    fallback_path_bucket_digests: Vec<String>,
    #[serde(default)]
    missing_path_bucket_digests: Vec<String>,
    #[serde(default)]
    path_bucket_diagnostics: Vec<SummaryPathBucketDiagnostic>,
    #[serde(default)]
    last_material_attempt_generation_id: Option<String>,
    #[serde(default)]
    last_material_attempt_started_at: Option<String>,
    #[serde(default)]
    last_material_attempt_status: Option<String>,
    #[serde(default)]
    missing_path_bucket_count: usize,
    #[serde(default)]
    context_truncated: bool,
    #[serde(default)]
    included_source_count: usize,
    #[serde(default)]
    omitted_source_count: usize,
    // The canonical v2 contract keeps these fields flat.  Accept the nested
    // form as well because an early development build wrote it that way; the
    // reader must not make already-generated summaries disappear.
    #[serde(default)]
    path_bucket: Option<SummaryPathBucket>,
    #[serde(default)]
    path_bucket_kind: Option<String>,
    #[serde(default)]
    path_label: Option<String>,
}

impl V2SummaryMetadata {
    fn path_bucket(&self, digest: Option<&str>) -> Option<SummaryPathBucket> {
        self.path_bucket.clone().or_else(|| {
            Some(SummaryPathBucket {
                path_bucket_digest: digest?.to_string(),
                path_bucket_kind: self.path_bucket_kind.clone()?,
                path_label: self.path_label.clone(),
            })
        })
    }
}

/// Decomposed `external_id`. `period_key`/`scope_key` are set for period
/// summaries; `thread_id` is set for per-thread summaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedSummaryExternalId {
    pub kind: SummaryKind,
    /// `2026-05-24` / `2026-W21` / `2026-05`; `None` for per-thread.
    pub period_key: Option<String>,
    /// e.g. `_all`; `None` for per-thread.
    pub scope_key: Option<String>,
    /// Set only for per-thread summaries.
    pub thread_id: Option<i64>,
    /// Set only for group summaries.
    pub group_id: Option<i64>,
    /// Set only for v2 period summaries.
    pub rollup_scope: Option<RollupScope>,
    /// The opaque v2 Path identity for a Path rollup.
    pub path_bucket_digest: Option<String>,
    /// True for a pre-v2 period summary whose aggregation layout is unknown.
    pub legacy: bool,
}

/// Parse a summary `external_id` into its kind and components. Returns `None`
/// for shapes that don't match any known prefix so callers fall back to
/// displaying the raw id.
pub(crate) fn parse_summary_external_id(ext: &str) -> Option<ParsedSummaryExternalId> {
    if let Some(group_id) = super::parse_i64_after_prefix("thread-group-summary:", Some(ext)) {
        return Some(ParsedSummaryExternalId {
            kind: SummaryKind::ThreadGroup,
            period_key: None,
            scope_key: None,
            thread_id: None,
            group_id: Some(group_id),
            rollup_scope: None,
            path_bucket_digest: None,
            legacy: false,
        });
    }
    if let Some(thread_id) = super::parse_i64_after_prefix("summary:", Some(ext)) {
        return Some(ParsedSummaryExternalId {
            kind: SummaryKind::PerThread,
            period_key: None,
            scope_key: None,
            thread_id: Some(thread_id),
            group_id: None,
            rollup_scope: None,
            path_bucket_digest: None,
            legacy: false,
        });
    }
    for kind in [
        SummaryKind::Daily,
        SummaryKind::Weekly,
        SummaryKind::Monthly,
    ] {
        if let Some(rest) = ext.strip_prefix(kind.external_id_namespace()) {
            if let Some(v2) = rest.strip_prefix("v2:") {
                let parts: Vec<&str> = v2.split(':').collect();
                // v2 forms are `<owner>:<period>:<source-digest>:overall`
                // and `<owner>:<period>:<source-digest>:path:<path-digest>`.
                // Every delimiter-separated component is deliberately opaque
                // except owner; source/path values are fixed digests.
                let (owner, period_key, source_scope_digest, rollup_scope, path_bucket_digest) =
                    match parts.as_slice() {
                        [owner, period, source_scope, "overall"] => {
                            (*owner, *period, *source_scope, RollupScope::Overall, None)
                        }
                        [owner, period, source_scope, "path", path_digest] => (
                            *owner,
                            *period,
                            *source_scope,
                            RollupScope::Path,
                            Some((*path_digest).to_string()),
                        ),
                        _ => continue,
                    };
                if owner.parse::<i64>().is_err()
                    || period_key.is_empty()
                    || source_scope_digest.is_empty()
                    || path_bucket_digest.as_deref().is_some_and(str::is_empty)
                {
                    continue;
                }
                return Some(ParsedSummaryExternalId {
                    kind,
                    period_key: Some(period_key.to_string()),
                    scope_key: Some(source_scope_digest.to_string()),
                    thread_id: None,
                    group_id: None,
                    rollup_scope: Some(rollup_scope),
                    path_bucket_digest,
                    legacy: false,
                });
            }
            // Current memories IDs are `<namespace>:<owner>:<period>:<scope>`.
            // Accept the owner-less predecessor as well so an ID convention
            // change cannot make otherwise correctly typed summaries vanish.
            let (first, remainder) = rest.split_once(':')?;
            let (period_key, scope_key) = if first.parse::<i64>().is_ok() {
                remainder.split_once(':')?
            } else {
                (first, remainder)
            };
            if period_key.is_empty() {
                return None;
            }
            return Some(ParsedSummaryExternalId {
                kind,
                period_key: Some(period_key.to_string()),
                scope_key: Some(scope_key.to_string()),
                thread_id: None,
                group_id: None,
                rollup_scope: None,
                path_bucket_digest: None,
                legacy: true,
            });
        }
    }
    None
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryEntry {
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
    #[serde(with = "crate::serde_id::option")]
    pub thread_id: Option<i64>,
    #[serde(with = "crate::serde_id::option")]
    pub group_id: Option<i64>,
    pub external_id: Option<String>,
    pub kind: SummaryKind,
    /// `2026-05-24` / `2026-W21` / `2026-05`; `None` for per-thread.
    pub period_key: Option<String>,
    /// Project/team scope (`_all` by default); `None` for per-thread. The
    /// same `period_key` can have several scopes, so the UI keys cards on
    /// `(period_key, scope_key)`.
    pub scope_key: Option<String>,
    pub content_json: String,
    pub updated_at_ms: i64,
    pub rollup_scope: Option<RollupScope>,
    pub source_scope_digest: Option<String>,
    pub path_bucket: Option<SummaryPathBucket>,
    pub aggregation_mode: Option<String>,
    pub completion_status: Option<CompletionStatus>,
    pub generation_id: Option<String>,
    pub generation_started_at: Option<String>,
    pub generation_consistency: Option<String>,
    pub input_generation_ids: Option<Vec<String>>,
    pub input_without_generation_id_count: Option<usize>,
    pub failed_path_bucket_digests: Vec<String>,
    pub fallback_path_bucket_digests: Vec<String>,
    pub missing_path_bucket_digests: Vec<String>,
    pub path_bucket_diagnostics: Vec<SummaryPathBucketDiagnostic>,
    pub last_material_attempt_generation_id: Option<String>,
    pub last_material_attempt_started_at: Option<String>,
    pub last_material_attempt_status: Option<String>,
    /// Present for v2 overall rows when the planned path set was larger than
    /// the successfully available child summaries.
    pub missing_path_bucket_count: Option<usize>,
    pub context_truncated: Option<bool>,
    pub included_source_count: Option<usize>,
    pub omitted_source_count: Option<usize>,
    /// Number of Path-scope children sharing this v2 Overall row's period
    /// and source scope. Computed for the list response, never persisted.
    pub path_summary_count: Option<usize>,
    pub legacy: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListSummariesRequest {
    /// Defaults to the public ThreadGroup view. Per-thread rows are internal
    /// inputs for period and group generation, not a public list namespace.
    #[serde(default)]
    pub kind: SummaryKind,
    pub limit: Option<i32>,
    pub offset: Option<i64>,
    pub created_after_ms: Option<i64>,
    pub created_before_ms: Option<i64>,
    /// `memory.updated_at` window. For period summaries this carries the
    /// original aggregation time, so it filters by the period the work
    /// actually happened in (the summary *thread* updated_at gets bumped to
    /// the rerun time, which is why we filter on the memory, not the thread).
    pub updated_after_ms: Option<i64>,
    pub updated_before_ms: Option<i64>,
    /// Exact period token from a period summary external ID. Calendar detail
    /// queries use this instead of a timestamp window.
    pub period_key: Option<String>,
    // FR-SUM-2a fixes AND-only matching for the per-thread label filter, so
    // (unlike threads.rs's ListThreadsRequest) there is no OR mode to select.
    #[serde(default)]
    pub labels_any: Vec<String>,
    /// v2 period rollup scopes. Omitted period requests preserve the normal
    /// Overall-only view; per-thread requests ignore this compatibility field.
    #[serde(default)]
    pub rollup_scopes: Vec<RollupScope>,
    /// Opaque Path identities. These apply only to v2 Path summaries.
    #[serde(default)]
    pub path_bucket_digests: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListSummariesForSelectionRequest {
    #[serde(default)]
    pub kind: SummaryKind,
    pub limit: Option<i32>,
    pub offset: Option<i64>,
    #[serde(default)]
    pub rollup_scopes: Vec<RollupScope>,
    #[serde(default)]
    pub path_bucket_digests: Vec<String>,
    #[serde(default)]
    pub include_shadowed_legacy: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummarySelectionPage {
    pub entries: Vec<SummaryEntry>,
    pub next_offset: Option<i64>,
}

/// A server-authorized Path filter candidate for the chat selection modal.
/// The opaque digest is the only value the client sends back in list/count
/// filters; the full label is returned solely for rendering after the normal
/// owner/kind query has established authorization.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SummaryPathCandidate {
    pub path_bucket: SummaryPathBucket,
    pub count: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListSummaryPathCandidatesRequest {
    pub kind: SummaryKind,
    #[serde(default)]
    pub period_key: Option<String>,
    #[serde(default)]
    pub source_scope_digest: Option<String>,
}

#[tauri::command]
pub async fn list_summary_path_candidates(
    state: State<'_, AppState>,
    req: ListSummaryPathCandidatesRequest,
) -> AppResult<Vec<SummaryPathCandidate>> {
    if req.kind == SummaryKind::PerThread {
        return Err(crate::error::AppError::Config(
            "path candidates are only available for period summaries".into(),
        ));
    }
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);
    let mut stream = client
        .find_list_by_condition(build_summary_memory_request_for(req.kind))
        .await?
        .into_inner();
    let mut candidates: BTreeMap<String, SummaryPathCandidate> = BTreeMap::new();
    while let Some(item) = stream.next().await {
        let Some(summary) = entry_to_summary(item?, req.kind) else {
            continue;
        };
        if !matches_path_scope(
            &summary,
            req.period_key.as_deref(),
            req.source_scope_digest.as_deref(),
        ) {
            continue;
        }
        let Some(bucket) = summary.path_bucket else {
            continue;
        };
        candidates
            .entry(bucket.path_bucket_digest.clone())
            .and_modify(|candidate| candidate.count += 1)
            .or_insert(SummaryPathCandidate {
                path_bucket: bucket,
                count: 1,
            });
    }
    let mut candidates: Vec<_> = candidates.into_values().collect();
    candidates.sort_by(|left, right| {
        let left_label = left.path_bucket.path_label.as_deref().unwrap_or("~");
        let right_label = right.path_bucket.path_label.as_deref().unwrap_or("~");
        left_label.cmp(right_label)
    });
    Ok(candidates)
}

/// Lightweight list of Path children for an Overall period card. Content and
/// unrelated period metadata are deliberately omitted; the UI fetches a
/// child's complete body only after that child is expanded.
#[derive(Debug, Clone, Deserialize)]
pub struct ListSummaryPathChildrenRequest {
    pub kind: SummaryKind,
    pub period_key: String,
    pub source_scope_digest: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryPathChildEntry {
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
    pub path_bucket: SummaryPathBucket,
    pub aggregation_mode: Option<String>,
}

#[tauri::command]
pub async fn list_summary_path_children(
    state: State<'_, AppState>,
    req: ListSummaryPathChildrenRequest,
) -> AppResult<Vec<SummaryPathChildEntry>> {
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);
    let mut stream = client
        .find_list_by_condition(build_summary_memory_request_for(req.kind))
        .await?
        .into_inner();
    let mut children = Vec::new();
    while let Some(item) = stream.next().await {
        if let Some(summary) = entry_to_summary(item?, req.kind)
            && matches_path_scope(
                &summary,
                Some(&req.period_key),
                Some(&req.source_scope_digest),
            )
            && let Some(child) = path_child_read_model(summary)
        {
            children.push(child);
        }
    }
    children.sort_by(|left, right| {
        path_bucket_sort_key(&left.path_bucket).cmp(&path_bucket_sort_key(&right.path_bucket))
    });
    Ok(children)
}

fn matches_path_scope(
    summary: &SummaryEntry,
    period_key: Option<&str>,
    source_scope_digest: Option<&str>,
) -> bool {
    summary.rollup_scope == Some(RollupScope::Path)
        && period_key.is_none_or(|period| summary.period_key.as_deref() == Some(period))
        && source_scope_digest
            .is_none_or(|scope| summary.source_scope_digest.as_deref() == Some(scope))
}

fn path_child_read_model(summary: SummaryEntry) -> Option<SummaryPathChildEntry> {
    Some(SummaryPathChildEntry {
        memory_id: summary.memory_id,
        path_bucket: summary.path_bucket?,
        aggregation_mode: summary.aggregation_mode,
    })
}

fn path_bucket_sort_key(bucket: &SummaryPathBucket) -> (u8, &str) {
    match bucket {
        SummaryPathBucket {
            path_bucket_kind,
            path_label: Some(path_label),
            ..
        } if path_bucket_kind == "label" => (0, path_label.as_str()),
        SummaryPathBucket {
            path_bucket_kind, ..
        } if path_bucket_kind == "ambiguous" => (1, ""),
        _ => (2, ""),
    }
}

/// Build the `FindListByCondition` request for a summaries list query. Pure
/// so the wire-shape (prefix selection, date windows) is unit-tested.
fn build_find_request(req: &ListSummariesRequest) -> mem_svc::FindMemoryListRequest {
    let mut request = build_summary_memory_request_for(req.kind);
    request.limit = req.limit;
    request.offset = req.offset;
    request.updated_after = req.updated_after_ms;
    request.updated_before = req.updated_before_ms;
    request.thread_filter = (!req.labels_any.is_empty()).then(|| mem_data::ThreadSearchFilter {
        user_id: Some(LOOKBACK_USER_ID),
        labels: req.labels_any.clone(),
        label_match_mode: Some(LabelMatch::All.to_proto()),
        channel: None,
        thread_created_after: None,
        thread_created_before: None,
        thread_updated_after: None,
        thread_updated_before: None,
        first_message_after: None,
        first_message_before: None,
        last_message_after: None,
        last_message_before: None,
        memory_kinds: vec![summary_memory_kind(req.kind)],
        thread_group_id: None,
        thread_id: None,
    });
    request.created_after = req.created_after_ms;
    request.created_before = req.created_before_ms;
    request
}

/// Build the common owner and summary-kind scope. Callers overlay only the
/// filters that are meaningful for their query shape.
fn build_summary_memory_request_for(kind: SummaryKind) -> mem_svc::FindMemoryListRequest {
    mem_svc::FindMemoryListRequest {
        limit: None,
        offset: None,
        roles: vec![],
        user_id: Some(mem_data::UserId {
            value: LOOKBACK_USER_ID,
        }),
        thread_id: None,
        updated_after: None,
        updated_before: None,
        external_id: None,
        content_types: vec![],
        thread_filter: None,
        created_after: None,
        created_before: None,
        sort: None,
        // Filter by namespace before applying pagination.  The memory kind
        // alone is not sufficient on older memories databases where rows
        // with a stale external_id can otherwise share the same kind.
        external_id_prefix: Some(kind.external_id_namespace().to_string()),
        memory_kinds: vec![summary_memory_kind(kind)],
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FindSummaryCoOccurringLabelsRequest {
    #[serde(default)]
    pub labels: Vec<String>,
}

/// Return all labels on per-thread summary threads except labels that describe
/// the summary storage layout rather than the source conversation category.
#[tauri::command]
pub async fn find_summary_distinct_labels(
    state: State<'_, AppState>,
) -> AppResult<Vec<LabelWithCount>> {
    let labels = summary_thread_labels(&state, &[]).await?;
    Ok(aggregate_summary_labels(labels))
}

/// Return labels that occur with every requested user label on per-thread
/// summaries. Query labels are omitted because LabelFilter keeps the selected
/// chips visible from its distinct-label input.
#[tauri::command]
pub async fn find_summary_co_occurring_labels(
    state: State<'_, AppState>,
    req: FindSummaryCoOccurringLabelsRequest,
) -> AppResult<Vec<LabelWithCount>> {
    let labels = summary_thread_labels(&state, &req.labels).await?;
    Ok(aggregate_summary_cooccurring_labels(labels, &req.labels))
}

/// Streams every matching thread and summary Memory instead of relying on the
/// memories-side label aggregate RPCs (`ThreadService.FindDistinctLabels` /
/// `FindCoOccurringLabels`, see `threads.rs`). Neither aggregate can express
/// the required join: labels must come only from threads that still have a
/// live per-thread summary Memory row. The Memory query deliberately has no
/// `thread_filter`; resolving every summary thread there would hit the
/// server's `MEMORY_THREAD_FILTER_MAX_THREAD_IDS` limit for large histories.
async fn summary_thread_labels(
    state: &State<'_, AppState>,
    selected: &[String],
) -> AppResult<Vec<Vec<String>>> {
    let threads = summary_threads(state, selected).await?;
    let memory_thread_ids = summary_memory_thread_ids(state).await?;
    Ok(labels_for_summary_memories(threads, memory_thread_ids))
}

#[derive(Debug)]
struct SummaryThreadLabels {
    id: i64,
    labels: Vec<String>,
}

async fn summary_threads(
    state: &State<'_, AppState>,
    selected: &[String],
) -> AppResult<Vec<SummaryThreadLabels>> {
    let mut client = ThreadServiceClient::new(state.memories_channel().await?);
    let labels = summary_filter_labels(selected);
    let request = mem_svc::FindThreadListByLabelsRequest {
        labels,
        match_mode: Some(LabelMatch::All.to_proto()),
        limit: None,
        offset: None,
        user_id: Some(LOOKBACK_USER_ID),
        thread_created_after: None,
        thread_created_before: None,
        thread_updated_after: None,
        thread_updated_before: None,
        first_message_after: None,
        first_message_before: None,
        last_message_after: None,
        last_message_before: None,
        sort: None,
        memory_kinds: vec![summary_memory_kind(SummaryKind::PerThread)],
    };
    let mut stream = client
        .find_thread_list_by_labels(request)
        .await?
        .into_inner();
    let mut out = Vec::new();
    while let Some(thread) = stream.next().await {
        let thread = thread?;
        if let (Some(id), Some(data)) = (thread.id, thread.data) {
            out.push(SummaryThreadLabels {
                id: id.value,
                labels: data.labels,
            });
        }
    }
    Ok(out)
}

/// Finds backing summary Memory rows independently from the thread list so
/// empty threads left by deletion or label-migration cleanup never surface as
/// filter candidates.
async fn summary_memory_thread_ids(state: &State<'_, AppState>) -> AppResult<HashSet<i64>> {
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);
    let mut stream = client
        .find_list_by_condition(build_summary_memory_request())
        .await?
        .into_inner();
    let mut ids = HashSet::new();
    while let Some(entry) = stream.next().await {
        if let Some(thread_id) = entry?.thread_id {
            ids.insert(thread_id.value);
        }
    }
    Ok(ids)
}

/// Builds the backing-Memory query used for label aggregation. Label matching
/// occurs in `summary_threads`; keeping this query free of a thread filter
/// prevents the server from expanding every summary thread ID in one request.
fn build_summary_memory_request() -> mem_svc::FindMemoryListRequest {
    build_summary_memory_request_for(SummaryKind::PerThread)
}

fn summary_filter_labels(selected: &[String]) -> Vec<String> {
    let mut labels = Vec::with_capacity(selected.len() + 1);
    labels.push("summary".to_string());
    labels.extend(selected.iter().cloned());
    labels.sort();
    labels.dedup();
    labels
}

fn labels_for_summary_memories<I>(
    threads: I,
    memory_thread_ids: impl IntoIterator<Item = i64>,
) -> Vec<Vec<String>>
where
    I: IntoIterator<Item = SummaryThreadLabels>,
{
    let memory_thread_ids: HashSet<i64> = memory_thread_ids.into_iter().collect();
    threads
        .into_iter()
        .filter(|thread| memory_thread_ids.contains(&thread.id))
        .map(|thread| thread.labels)
        .collect()
}

fn aggregate_summary_labels<I>(thread_labels: I) -> Vec<LabelWithCount>
where
    I: IntoIterator<Item = Vec<String>>,
{
    let mut counts = BTreeMap::<String, i64>::new();
    for labels in thread_labels {
        for label in labels {
            if is_summary_structural_label(&label) {
                continue;
            }
            *counts.entry(label).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .map(|(label, thread_count)| LabelWithCount {
            label,
            thread_count,
        })
        .collect()
}

fn aggregate_summary_cooccurring_labels<I>(
    thread_labels: I,
    selected: &[String],
) -> Vec<LabelWithCount>
where
    I: IntoIterator<Item = Vec<String>>,
{
    let selected: HashSet<&str> = selected.iter().map(String::as_str).collect();
    aggregate_summary_labels(thread_labels)
        .into_iter()
        .filter(|label| !selected.contains(label.label.as_str()))
        .collect()
}

/// Mirrors the label vocabulary minted by the `*-work-summary` workflow YAMLs
/// and documented as canonical in `workers/workflows/rag/lookback-recall.yaml`
/// (`summary_labels`); keep in sync if that vocabulary changes.
fn is_summary_structural_label(label: &str) -> bool {
    matches!(
        label,
        "summary" | "thread_group_summary" | "daily_summary" | "weekly_summary" | "monthly_summary"
    ) || [
        "date:",
        "iso_week:",
        "month:",
        "scope:",
        "summary_scope:",
        "summary_path:",
    ]
    .iter()
    .any(|prefix| label.starts_with(prefix))
}

#[tauri::command]
pub async fn list_summaries(
    state: State<'_, AppState>,
    req: ListSummariesRequest,
) -> AppResult<Vec<SummaryEntry>> {
    if req.kind == SummaryKind::PerThread {
        return Ok(Vec::new());
    }
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);

    let mut request = build_find_request(&req);
    // Memories paginates before it can know our v2 metadata scope.  Scan the
    // namespace first, then apply the caller's page after the local v2
    // predicate; otherwise a Path-heavy period can make an Overall list page
    // appear empty.
    let needs_local_period_scope = is_period_kind(req.kind);
    if needs_local_period_scope {
        request.limit = None;
        request.offset = None;
        // Count Path rows over the complete period/scope before applying the
        // date window to visible rows. The lazy child endpoint has no date
        // filter, so counting only date-matching children could hide a
        // disclosure that still has several reachable children.
        request.updated_after = None;
        request.updated_before = None;
    }
    let mut stream = client.find_list_by_condition(request).await?.into_inner();
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let entry = item?;
        // The memories server applies both `memory_kinds` and
        // `external_id_prefix` before limit/offset. Remote targets missing
        // either field are rejected by the reflection compatibility gate, so
        // a current server cannot consume a page with an invalid-kind row.
        // Keep this post-filter as a defense for malformed/legacy rows.
        if let Some(s) = entry_to_summary(entry, req.kind)
            && req
                .period_key
                .as_deref()
                .is_none_or(|key| s.period_key.as_deref() == Some(key))
        {
            out.push(s);
        }
    }
    // Attach counts before the date window is applied: Path children can be
    // older than their visible Overall row, but the lazy child endpoint still
    // exposes them. Apply legacy precedence only after the visible window is
    // selected, preserving the list's original date-filter semantics.
    let out = annotate_path_summary_counts(out);
    let out: Vec<_> = out
        .into_iter()
        .filter(|entry| {
            summary_matches_updated_window(entry.updated_at_ms, &req)
                && summary_matches_rollup_filter(
                    entry,
                    &req.rollup_scopes,
                    &req.path_bucket_digests,
                )
        })
        .collect();
    // Legacy `_all` rows remain the only usable representation for an older
    // period. Once the default-scope v2 overall is complete, however, the
    // legacy row is merely a previous representation of the same period and
    // must not appear alongside the canonical Path-derived result.
    let out = remove_shadowed_legacy(out, false);
    if needs_local_period_scope {
        let offset = req.offset.unwrap_or(0).max(0) as usize;
        let limit = req.limit.unwrap_or(i32::MAX).max(0) as usize;
        Ok(out.into_iter().skip(offset).take(limit).collect())
    } else {
        Ok(out)
    }
}

/// Apply a period-list date window after the complete period has been scanned
/// for Path-count metadata. The public UI documents both bounds as strict.
fn summary_matches_updated_window(updated_at_ms: i64, req: &ListSummariesRequest) -> bool {
    req.updated_after_ms
        .is_none_or(|after| updated_at_ms > after)
        && req
            .updated_before_ms
            .is_none_or(|before| updated_at_ms < before)
}

/// Attach the already-scanned number of Path children to each Overall row.
/// This keeps the UI's disclosure lazy while letting it decide whether an
/// one-child breakdown is meaningful before it is opened.
fn annotate_path_summary_counts(mut entries: Vec<SummaryEntry>) -> Vec<SummaryEntry> {
    let mut counts: HashMap<(String, String, String), usize> = HashMap::new();
    for entry in &entries {
        if !entry.legacy
            && entry.rollup_scope == Some(RollupScope::Path)
            && let (Some(period), Some(scope)) = (&entry.period_key, &entry.source_scope_digest)
        {
            *counts
                .entry((
                    entry.kind.external_id_namespace().to_string(),
                    period.clone(),
                    scope.clone(),
                ))
                .or_default() += 1;
        }
    }
    for entry in &mut entries {
        if !entry.legacy
            && entry.rollup_scope == Some(RollupScope::Overall)
            && let (Some(period), Some(scope)) = (&entry.period_key, &entry.source_scope_digest)
        {
            entry.path_summary_count = Some(
                counts
                    .get(&(
                        entry.kind.external_id_namespace().to_string(),
                        period.clone(),
                        scope.clone(),
                    ))
                    .copied()
                    .unwrap_or(0),
            );
        }
    }
    entries
}

/// Retain legacy-only periods while suppressing only a legacy `_all` summary
/// that a complete default-scope v2 overall has superseded.  Old named scopes
/// cannot be mapped safely to v2's match-mode digest, so they are never
/// hidden automatically.  `include_shadowed` is used by the explicit
/// compatibility view in the summary-selection modal.
fn remove_shadowed_legacy(entries: Vec<SummaryEntry>, include_shadowed: bool) -> Vec<SummaryEntry> {
    if include_shadowed {
        return entries;
    }
    let default_scope_digest = source_scope_digest(&[], Some("LABEL_ALL"));
    let completed_default_overalls: HashSet<(String, String)> = entries
        .iter()
        .filter(|entry| {
            !entry.legacy
                && entry.rollup_scope == Some(RollupScope::Overall)
                && entry.completion_status == Some(CompletionStatus::Complete)
                && entry.source_scope_digest.as_deref() == Some(default_scope_digest.as_str())
        })
        .filter_map(|entry| {
            entry.period_key.as_deref().map(|period_key| {
                (
                    entry.kind.external_id_namespace().to_string(),
                    period_key.to_string(),
                )
            })
        })
        .collect();
    entries
        .into_iter()
        .filter(|entry| {
            !(entry.legacy
                && entry.scope_key.as_deref() == Some("_all")
                && entry.period_key.as_deref().is_some_and(|period_key| {
                    completed_default_overalls.contains(&(
                        entry.kind.external_id_namespace().to_string(),
                        period_key.to_string(),
                    ))
                }))
        })
        .collect()
}

fn summary_matches_rollup_filter(
    entry: &SummaryEntry,
    requested_scopes: &[RollupScope],
    path_bucket_digests: &[String],
) -> bool {
    if matches!(
        entry.kind,
        SummaryKind::PerThread | SummaryKind::ThreadGroup
    ) {
        return true;
    }
    // A legacy period row has no provable v2 Path identity. It remains
    // available in non-Path compatibility views, but must never satisfy a
    // selected Path merely because it predates the v2 metadata.
    if entry.legacy {
        return path_bucket_digests.is_empty() && !requested_scopes.contains(&RollupScope::Path);
    }
    let scopes = if requested_scopes.is_empty() {
        &[RollupScope::Overall][..]
    } else {
        requested_scopes
    };
    if !entry
        .rollup_scope
        .is_some_and(|scope| scopes.contains(&scope))
    {
        return false;
    }
    path_bucket_digests.is_empty()
        || entry.rollup_scope == Some(RollupScope::Overall)
        || entry
            .path_bucket
            .as_ref()
            .is_some_and(|path| path_bucket_digests.contains(&path.path_bucket_digest))
}

/// Selection-modal pagination keeps the server-side raw offset independent
/// from the number of rows that survive defensive kind/namespace validation.
/// The public summaries page continues to use `list_summaries` unchanged.
#[tauri::command]
pub async fn list_summaries_for_selection(
    state: State<'_, AppState>,
    req: ListSummariesForSelectionRequest,
) -> AppResult<SummarySelectionPage> {
    if req.kind == SummaryKind::PerThread {
        return Ok(SummarySelectionPage {
            entries: Vec::new(),
            next_offset: None,
        });
    }
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);
    let page_limit = req.limit.unwrap_or(50).max(1);
    let mut list_request = build_find_request(&ListSummariesRequest {
        kind: req.kind,
        limit: Some(page_limit.saturating_add(1)),
        offset: req.offset,
        rollup_scopes: req.rollup_scopes.clone(),
        path_bucket_digests: req.path_bucket_digests.clone(),
        ..Default::default()
    });
    // See list_summaries: selection filters operate on parsed v2 metadata,
    // so an RPC page cannot be the user-visible page boundary.
    if is_period_kind(req.kind) {
        list_request.limit = None;
        list_request.offset = None;
    }
    // Per-thread requests are server-paged. Keep one raw sentinel solely for
    // continuation; malformed rows in the visible raw page must not make the
    // client conclude that a later server page does not exist.
    let mut stream = client
        .find_list_by_condition(list_request)
        .await?
        .into_inner();
    let mut raw_rows = Vec::new();
    while let Some(item) = stream.next().await {
        raw_rows.push(item?);
    }
    let applies_local_offset = is_period_kind(req.kind);
    let server_next_offset = (!applies_local_offset)
        .then(|| per_thread_server_next_offset(raw_rows.len(), page_limit, req.offset.unwrap_or(0)))
        .flatten();
    if !applies_local_offset {
        // Do not render the `limit + 1` sentinel: the next server page starts
        // at this raw boundary, and retaining it would duplicate an otherwise
        // valid row after filtering.
        raw_rows.truncate(page_limit as usize);
    }
    let mut page = summary_selection_page_from_raw(
        raw_rows,
        req.kind,
        page_limit,
        if applies_local_offset {
            req.offset.unwrap_or(0)
        } else {
            0
        },
        &req.rollup_scopes,
        &req.path_bucket_digests,
        req.include_shadowed_legacy,
    );
    // Non-period rows are already paged by Memories. The local parser is
    // defensive only, so applying the caller offset a second time would drop
    // every page after the first.
    if !applies_local_offset {
        page.next_offset = server_next_offset;
    }
    Ok(page)
}

fn per_thread_server_next_offset(
    raw_row_count: usize,
    page_limit: i32,
    offset: i64,
) -> Option<i64> {
    let limit = page_limit.max(1) as usize;
    (raw_row_count > limit).then(|| offset.max(0).saturating_add(limit as i64))
}

fn summary_selection_page_from_raw(
    raw_rows: Vec<mem_svc::MemoryListEntry>,
    expected_kind: SummaryKind,
    page_limit: i32,
    offset: i64,
    rollup_scopes: &[RollupScope],
    path_bucket_digests: &[String],
    include_legacy: bool,
) -> SummarySelectionPage {
    let limit = page_limit.max(1) as usize;
    let filtered: Vec<_> = raw_rows
        .into_iter()
        .filter_map(|row| entry_to_summary(row, expected_kind))
        // The modal's explicit compatibility toggle controls every legacy
        // row, not just legacy rows shadowed by a v2 Overall. A Path filter
        // below still excludes legacy rows even when this is enabled because
        // their Path identity cannot be established.
        .filter(|entry| include_legacy || !entry.legacy)
        .filter(|entry| summary_matches_rollup_filter(entry, rollup_scopes, path_bucket_digests))
        .collect();
    let filtered = remove_shadowed_legacy(filtered, include_legacy);
    let offset = offset.max(0) as usize;
    let has_more = filtered.len() > offset.saturating_add(limit);
    let entries = filtered.into_iter().skip(offset).take(limit).collect();
    SummarySelectionPage {
        entries,
        next_offset: has_more.then(|| (offset + limit) as i64),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetSummaryContentRequest {
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
}

/// Fetch the authoritative content for one summary selected from a list row.
/// List responses may be compacted for browsing, so selection must not create
/// a chat snapshot from their `content_json` excerpt.
#[tauri::command]
pub async fn get_summary_content(
    state: State<'_, AppState>,
    req: GetSummaryContentRequest,
) -> AppResult<Option<String>> {
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);
    let response = client
        .find(mem_data::MemoryId {
            value: req.memory_id,
        })
        .await?
        .into_inner();
    // `Find` is an ID lookup and therefore does not inherit the owner filter
    // used by the list endpoint. Never expose a summary just because its ID
    // was guessed or supplied by another user.
    Ok(response.data.and_then(|memory| {
        (!is_per_thread_summary(&memory))
            .then(|| summary_content_for_owner(memory, LOOKBACK_USER_ID))
            .flatten()
    }))
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CountSummariesRequest {
    #[serde(default)]
    pub kind: SummaryKind,
}

#[tauri::command]
pub async fn count_summaries(
    state: State<'_, AppState>,
    req: CountSummariesRequest,
) -> AppResult<i64> {
    if req.kind == SummaryKind::PerThread {
        return Ok(0);
    }
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);
    // CountByCondition cannot express the v2 rollup scope.  Counting through
    // the same parser/filter used by the visible list is intentionally more
    // expensive, but avoids presenting every Path child as an independent
    // period in the sidebar badge.
    let mut stream = client
        .find_list_by_condition(build_summary_memory_request_for(req.kind))
        .await?
        .into_inner();
    let mut total = 0;
    while let Some(item) = stream.next().await {
        if let Some(entry) = entry_to_summary(item?, req.kind)
            && summary_matches_rollup_filter(&entry, &[], &[])
        {
            total += 1;
        }
    }
    Ok(total)
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListSummaryPeriodKeysRequest {
    pub kind: SummaryKind,
    /// Prefixes for period tokens drawn in the shown calendar month. Daily
    /// and monthly use one prefix; weekly needs every ISO week touching it.
    #[serde(default)]
    pub period_key_prefixes: Vec<String>,
}

/// Return the sorted, de-duplicated set of `period_key`s that have a summary
/// matching the displayed calendar month. The membership comes from the
/// `external_id` period token rather than `updated_at`, because a period
/// summary timestamp is inherited from source memories and can fall outside
/// the token's calendar month.
#[tauri::command]
pub async fn list_summary_period_keys(
    state: State<'_, AppState>,
    req: ListSummaryPeriodKeysRequest,
) -> AppResult<Vec<String>> {
    if req.kind == SummaryKind::PerThread || req.kind == SummaryKind::ThreadGroup {
        return Ok(Vec::new());
    }
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);
    let mut keys: BTreeSet<String> = BTreeSet::new();
    let request = summary_period_keys_request(req.kind);
    let mut stream = client.find_list_by_condition(request).await?.into_inner();
    while let Some(item) = stream.next().await {
        if let Some(summary) = entry_to_summary(item?, req.kind)
            && summary_matches_rollup_filter(&summary, &[], &[])
            && let Some(period_key) = summary.period_key
            && req
                .period_key_prefixes
                .iter()
                .any(|prefix| period_key.starts_with(prefix))
        {
            keys.insert(period_key);
        }
    }
    Ok(keys.into_iter().collect())
}

// Kept as a pure wire-shape helper for callers/tests that only need the
// Memories count RPC contract. The UI-facing command intentionally streams
// rows above because this RPC has no rollup-scope predicate.
#[cfg(test)]
fn build_count_request(req: &CountSummariesRequest) -> mem_svc::MemoryCountCondition {
    mem_svc::MemoryCountCondition {
        roles: vec![],
        user_id: Some(mem_data::UserId {
            value: LOOKBACK_USER_ID,
        }),
        thread_id: None,
        updated_after: None,
        updated_before: None,
        external_id: None,
        content_types: vec![],
        thread_filter: None,
        created_after: None,
        created_before: None,
        external_id_prefix: Some(req.kind.external_id_namespace().to_string()),
        memory_kinds: vec![summary_memory_kind(req.kind)],
    }
}

fn summary_period_keys_request(kind: SummaryKind) -> mem_svc::FindMemoryListRequest {
    build_summary_memory_request_for(kind)
}

fn summary_memory_kind(kind: SummaryKind) -> i32 {
    match kind {
        SummaryKind::PerThread => mem_data::MemoryKind::ThreadSummary as i32,
        SummaryKind::ThreadGroup => mem_data::MemoryKind::DerivedSummary as i32,
        SummaryKind::Daily => mem_data::MemoryKind::DailySummary as i32,
        SummaryKind::Weekly => mem_data::MemoryKind::WeeklySummary as i32,
        SummaryKind::Monthly => mem_data::MemoryKind::MonthlySummary as i32,
    }
}

fn is_period_kind(kind: SummaryKind) -> bool {
    matches!(
        kind,
        SummaryKind::Daily | SummaryKind::Weekly | SummaryKind::Monthly
    )
}

/// Result of resolving a `source_memory_ids` chip click. The frontend uses
/// `kind` / `period_key` / `scope_key` / `thread_id` to decide whether to open
/// a conversation thread (per-thread) or focus the Summaries calendar on the
/// cited period card (daily/weekly/monthly). All fields are `Option` because
/// the memory may have been deleted, the `external_id` may not match a known
/// summary prefix (legacy / non-summary memory cited by mistake), or the
/// parsed per-thread id may be absent.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResolvedSummaryMemoryRef {
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
    #[serde(with = "crate::serde_id::option")]
    pub thread_id: Option<i64>,
    #[serde(with = "crate::serde_id::option")]
    pub group_id: Option<i64>,
    pub external_id: Option<String>,
    pub kind: Option<SummaryKind>,
    pub period_key: Option<String>,
    pub scope_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveSummaryMemoryRefRequest {
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
}

/// Look up a summary memory's `external_id` and parse it into navigation
/// coordinates. Returns `None` when the memory does not exist anymore (the
/// frontend renders a disabled chip + tooltip in that case). The thread id is
/// extracted from the per-thread `summary:<thread_id>` form; periodic
/// summaries return `thread_id: None` (their navigation target is a
/// Summaries-tab calendar focus, not a thread modal).
#[tauri::command]
pub async fn resolve_summary_memory_ref(
    state: State<'_, AppState>,
    req: ResolveSummaryMemoryRefRequest,
) -> AppResult<Option<ResolvedSummaryMemoryRef>> {
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);
    let resp = client
        .find(mem_data::MemoryId {
            value: req.memory_id,
        })
        .await?
        .into_inner();
    Ok(resp.data.and_then(memory_to_resolved_ref))
}

/// Flat tuple of the navigation coordinates pulled from a summary memory's
/// `external_id`. Shared by `entry_to_summary` (list rows) and
/// `memory_to_resolved_ref` (single-memory resolve) so both stay in sync.
#[derive(Debug, Default, Clone)]
struct SummaryCoords {
    kind: Option<SummaryKind>,
    period_key: Option<String>,
    scope_key: Option<String>,
    thread_id: Option<i64>,
    group_id: Option<i64>,
    rollup_scope: Option<RollupScope>,
    path_bucket_digest: Option<String>,
    legacy: bool,
}

fn summary_coords_from_external_id(external_id: Option<&str>) -> SummaryCoords {
    let Some(parsed) = external_id.and_then(parse_summary_external_id) else {
        return SummaryCoords::default();
    };
    SummaryCoords {
        kind: Some(parsed.kind),
        period_key: parsed.period_key,
        scope_key: parsed.scope_key,
        thread_id: parsed.thread_id,
        group_id: parsed.group_id,
        rollup_scope: parsed.rollup_scope,
        path_bucket_digest: parsed.path_bucket_digest,
        legacy: parsed.legacy,
    }
}

fn memory_to_resolved_ref(memory: mem_data::Memory) -> Option<ResolvedSummaryMemoryRef> {
    let memory_id = memory.id?.value;
    let data = memory.data?;
    let coords = summary_coords_from_external_id(data.external_id.as_deref());
    Some(ResolvedSummaryMemoryRef {
        memory_id,
        thread_id: coords.thread_id,
        group_id: coords.group_id,
        external_id: data.external_id,
        kind: coords.kind,
        period_key: coords.period_key,
        scope_key: coords.scope_key,
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteSummaryRequest {
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
}

/// Delete a single summary. Summaries are Memory rows (owned by
/// `LOOKBACK_USER_ID`), so deletion is `/llm_memory.service.MemoryService/Delete`
/// by the backing `memory_id` — identical for per-thread / daily / weekly /
/// monthly. The UI gates this behind a confirm dialog.
#[tauri::command]
pub async fn delete_summary(
    state: State<'_, AppState>,
    req: DeleteSummaryRequest,
) -> AppResult<()> {
    let registered = super::begin_search_index_write(
        &state,
        &[crate::search_index_maintenance::MaintenanceTable::Memory],
    )
    .await?;
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);
    let result = client
        .delete(mem_data::MemoryId {
            value: req.memory_id,
        })
        .await
        .map(|_| ());
    let finish = super::finish_search_index_write(&state, registered).await;
    result?;
    finish
}

fn entry_to_summary(
    e: mem_svc::MemoryListEntry,
    expected_kind: SummaryKind,
) -> Option<SummaryEntry> {
    let memory = e.memory?;
    let data = memory.data?;
    // Reflection records have a dedicated selection tab.  Reject malformed
    // legacy rows even if their memory kind or external-id was accidentally
    // rewritten to look like a summary.
    if data.role == mem_data::MessageRole::RoleReflection as i32 {
        return None;
    }
    let memory_kind = mem_data::MemoryKind::try_from(data.memory_kind).ok()?;
    if memory_kind as i32 != summary_memory_kind(expected_kind) {
        return None;
    }
    let coords = summary_coords_from_external_id(data.external_id.as_deref());
    if coords.kind != Some(expected_kind) {
        return None;
    }
    let (
        rollup_scope,
        source_scope_digest,
        path_bucket,
        aggregation_mode,
        completion_status,
        generation_id,
        generation_started_at,
        generation_consistency,
        input_generation_ids,
        input_without_generation_id_count,
        failed_path_bucket_digests,
        fallback_path_bucket_digests,
        missing_path_bucket_digests,
        path_bucket_diagnostics,
        last_material_attempt_generation_id,
        last_material_attempt_started_at,
        last_material_attempt_status,
        missing_path_bucket_count,
        context_truncated,
        included_source_count,
        omitted_source_count,
    ) = if let Some(rollup_scope) = coords.rollup_scope {
        let metadata: V2SummaryMetadata = serde_json::from_str(data.metadata.as_deref()?).ok()?;
        let metadata_path_bucket = metadata.path_bucket(coords.path_bucket_digest.as_deref());
        match rollup_scope {
            RollupScope::Overall if metadata_path_bucket.is_some() => return None,
            RollupScope::Path => {
                let path_bucket = metadata_path_bucket.as_ref()?;
                if Some(path_bucket.path_bucket_digest.as_str())
                    != coords.path_bucket_digest.as_deref()
                {
                    return None;
                }
            }
            _ => {}
        }
        (
            Some(rollup_scope),
            coords.scope_key.clone(),
            metadata_path_bucket,
            Some(metadata.aggregation_mode),
            Some(metadata.completion_status),
            Some(metadata.generation_id),
            Some(metadata.generation_started_at),
            Some(metadata.generation_consistency),
            Some(metadata.input_generation_ids),
            Some(metadata.input_without_generation_id_count),
            metadata.failed_path_bucket_digests,
            metadata.fallback_path_bucket_digests,
            metadata.missing_path_bucket_digests,
            metadata.path_bucket_diagnostics,
            metadata.last_material_attempt_generation_id,
            metadata.last_material_attempt_started_at,
            metadata.last_material_attempt_status,
            Some(metadata.missing_path_bucket_count),
            Some(metadata.context_truncated),
            Some(metadata.included_source_count),
            Some(metadata.omitted_source_count),
        )
    } else {
        (
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    };
    Some(SummaryEntry {
        memory_id: memory.id?.value,
        thread_id: coords.thread_id,
        group_id: coords.group_id,
        external_id: data.external_id,
        kind: expected_kind,
        period_key: coords.period_key,
        scope_key: coords.scope_key,
        content_json: data.content,
        updated_at_ms: data.updated_at,
        rollup_scope,
        source_scope_digest,
        path_bucket,
        aggregation_mode,
        completion_status,
        generation_id,
        generation_started_at,
        generation_consistency,
        input_generation_ids,
        input_without_generation_id_count,
        failed_path_bucket_digests,
        fallback_path_bucket_digests,
        missing_path_bucket_digests,
        path_bucket_diagnostics,
        last_material_attempt_generation_id,
        last_material_attempt_started_at,
        last_material_attempt_status,
        missing_path_bucket_count,
        context_truncated,
        included_source_count,
        omitted_source_count,
        path_summary_count: None,
        legacy: coords.legacy,
    })
}

fn summary_content(memory: mem_data::Memory) -> Option<String> {
    let data = memory.data?;
    let kind = mem_data::MemoryKind::try_from(data.memory_kind).ok()?;
    matches!(
        kind,
        mem_data::MemoryKind::ThreadSummary
            | mem_data::MemoryKind::DailySummary
            | mem_data::MemoryKind::WeeklySummary
            | mem_data::MemoryKind::MonthlySummary
            | mem_data::MemoryKind::DerivedSummary
    )
    .then_some(data.content)
}

fn is_per_thread_summary(memory: &mem_data::Memory) -> bool {
    let Some(data) = memory.data.as_ref() else {
        return false;
    };
    mem_data::MemoryKind::try_from(data.memory_kind).ok()
        == Some(mem_data::MemoryKind::ThreadSummary)
        || data
            .external_id
            .as_deref()
            .and_then(parse_summary_external_id)
            .is_some_and(|coords| coords.kind == SummaryKind::PerThread)
}

fn summary_content_for_owner(memory: mem_data::Memory, owner_id: i64) -> Option<String> {
    let data = memory.data.as_ref()?;
    (data.user_id.as_ref()?.value == owner_id).then(|| summary_content(memory))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_period_summary_entry(rollup_scope: RollupScope, updated_at_ms: i64) -> SummaryEntry {
        SummaryEntry {
            memory_id: updated_at_ms,
            thread_id: None,
            group_id: None,
            external_id: None,
            kind: SummaryKind::Daily,
            period_key: Some("2026-08-01".into()),
            scope_key: Some("_all".into()),
            content_json: "{}".into(),
            updated_at_ms,
            rollup_scope: Some(rollup_scope),
            source_scope_digest: Some("scope".into()),
            path_bucket: None,
            aggregation_mode: None,
            completion_status: None,
            generation_id: None,
            generation_started_at: None,
            generation_consistency: None,
            input_generation_ids: None,
            input_without_generation_id_count: None,
            failed_path_bucket_digests: vec![],
            fallback_path_bucket_digests: vec![],
            missing_path_bucket_digests: vec![],
            path_bucket_diagnostics: vec![],
            last_material_attempt_generation_id: None,
            last_material_attempt_started_at: None,
            last_material_attempt_status: None,
            missing_path_bucket_count: None,
            context_truncated: None,
            included_source_count: None,
            omitted_source_count: None,
            path_summary_count: None,
            legacy: false,
        }
    }

    #[test]
    fn path_child_wire_row_omits_summary_body() {
        let child = SummaryPathChildEntry {
            memory_id: 42,
            path_bucket: SummaryPathBucket {
                path_bucket_digest: "bucket".into(),
                path_bucket_kind: "label".into(),
                path_label: Some("path:/workspace/project".into()),
            },
            aggregation_mode: Some("llm".into()),
        };
        let wire = serde_json::to_value(child).unwrap();
        assert_eq!(wire["memory_id"], "42");
        assert!(wire.get("content_json").is_none());
        assert!(wire.get("generation_id").is_none());
    }

    #[test]
    fn source_scope_digest_is_canonical_and_keeps_match_mode() {
        let labels = vec!["agent:codex".to_string(), "agent:claude".to_string()];
        let reversed = vec!["agent:claude".to_string(), "agent:codex".to_string()];
        assert_eq!(
            source_scope_digest(&labels, Some("LABEL_ALL")),
            source_scope_digest(&reversed, Some("LABEL_ALL"))
        );
        assert_ne!(
            source_scope_digest(&labels, Some("LABEL_ALL")),
            source_scope_digest(&labels, Some("LABEL_ANY"))
        );
        assert_eq!(
            source_scope_digest(&[], None),
            "999b928053067e9fc9369c99aa2ef91b5d096fabee5c34e90097fa937820524a"
        );
    }

    #[test]
    fn path_bucket_preserves_the_stored_label_and_reserves_special_cases() {
        let ordinary = PathBucket::from_thread_labels(&[
            "agent:codex".to_string(),
            "path:/workspace/project".to_string(),
        ]);
        assert_eq!(ordinary.kind(), "label");
        assert_eq!(ordinary.path_label(), Some("path:/workspace/project"));
        assert_eq!(
            ordinary.digest(),
            sha256_hex(r#"{"kind":"label","label":"path:/workspace/project","version":1}"#)
        );
        assert_eq!(PathBucket::from_thread_labels(&[]), PathBucket::Unassigned);
        assert_eq!(
            PathBucket::from_thread_labels(&[
                "path:/workspace/a".to_string(),
                "path:/workspace/b".to_string(),
            ]),
            PathBucket::Ambiguous
        );
        assert_eq!(
            path_bucket_digest("label", Some("path:/workspace/project")),
            Some(sha256_hex(
                r#"{"kind":"label","label":"path:/workspace/project","version":1}"#
            ))
        );
        assert_eq!(path_bucket_digest("label", None), None);
        assert_eq!(
            path_bucket_digest("invalid", Some("path:/workspace/project")),
            None
        );
    }

    #[test]
    fn delete_summary_request_deserializes_memory_id_from_json_string() {
        let req: DeleteSummaryRequest =
            serde_json::from_str(r#"{"memory_id":"9007199254740993"}"#).unwrap();
        assert_eq!(req.memory_id, 9_007_199_254_740_993);
    }

    #[test]
    fn get_summary_content_request_deserializes_memory_id_from_json_string() {
        let req: GetSummaryContentRequest =
            serde_json::from_str(r#"{"memory_id":"9007199254740993"}"#).unwrap();
        assert_eq!(req.memory_id, 9_007_199_254_740_993);
    }

    #[test]
    fn summary_content_accepts_only_summary_memory_kinds() {
        let summary = mem_data::Memory {
            id: None,
            data: Some(mem_data::MemoryData {
                content: "full content".into(),
                memory_kind: mem_data::MemoryKind::DailySummary as i32,
                ..Default::default()
            }),
            media: None,
        };
        assert_eq!(summary_content(summary), Some("full content".into()));

        let derived = mem_data::Memory {
            id: None,
            data: Some(mem_data::MemoryData {
                content: "derived summary".into(),
                memory_kind: mem_data::MemoryKind::DerivedSummary as i32,
                ..Default::default()
            }),
            media: None,
        };
        assert_eq!(summary_content(derived), Some("derived summary".into()));

        let raw = mem_data::Memory {
            id: None,
            data: Some(mem_data::MemoryData {
                content: "not a summary".into(),
                memory_kind: mem_data::MemoryKind::Raw as i32,
                ..Default::default()
            }),
            media: None,
        };
        assert_eq!(summary_content(raw), None);
    }

    #[test]
    fn summary_content_for_owner_rejects_other_or_missing_owners() {
        let owned = memory_with_external_id(1, Some("summary:1"));
        assert_eq!(
            summary_content_for_owner(owned, LOOKBACK_USER_ID),
            Some(String::new())
        );

        let mut other_owner = memory_with_external_id(2, Some("summary:2"));
        other_owner.data.as_mut().unwrap().user_id = Some(mem_data::UserId { value: 2 });
        assert_eq!(
            summary_content_for_owner(other_owner, LOOKBACK_USER_ID),
            None
        );

        let mut missing_owner = memory_with_external_id(3, Some("summary:3"));
        missing_owner.data.as_mut().unwrap().user_id = None;
        assert_eq!(
            summary_content_for_owner(missing_owner, LOOKBACK_USER_ID),
            None
        );
    }

    #[test]
    fn per_thread_summary_content_is_internal_even_without_a_parseable_external_id() {
        let internal = memory_with_kind(1, None, mem_data::MemoryKind::ThreadSummary);

        assert!(is_per_thread_summary(&internal));
    }

    #[test]
    fn parse_per_thread_external_id() {
        let p = parse_summary_external_id("summary:42").expect("parse");
        assert_eq!(p.kind, SummaryKind::PerThread);
        assert_eq!(p.thread_id, Some(42));
        assert_eq!(p.period_key, None);
        assert_eq!(p.scope_key, None);
    }

    #[test]
    fn parse_daily_external_id() {
        let p = parse_summary_external_id("daily:1:2026-05-24:_all").expect("parse");
        assert_eq!(p.kind, SummaryKind::Daily);
        assert_eq!(p.period_key.as_deref(), Some("2026-05-24"));
        assert_eq!(p.scope_key.as_deref(), Some("_all"));
        assert_eq!(p.thread_id, None);
    }

    #[test]
    fn parse_v2_path_period_external_id() {
        let p = parse_summary_external_id("daily:v2:1:2026-09-03:source-digest:path:path-digest")
            .expect("parse v2 path identity");
        assert_eq!(p.kind, SummaryKind::Daily);
        assert_eq!(p.period_key.as_deref(), Some("2026-09-03"));
        assert_eq!(p.scope_key.as_deref(), Some("source-digest"));
        assert_eq!(p.rollup_scope, Some(RollupScope::Path));
        assert_eq!(p.path_bucket_digest.as_deref(), Some("path-digest"));
        assert!(!p.legacy);
    }

    #[test]
    fn parse_v2_overall_period_external_id() {
        let p = parse_summary_external_id("weekly:v2:1:2026-W36:source-digest:overall")
            .expect("parse v2 overall identity");
        assert_eq!(p.rollup_scope, Some(RollupScope::Overall));
        assert_eq!(p.path_bucket_digest, None);
        assert!(!p.legacy);
    }

    #[test]
    fn v2_entry_retains_generation_and_failure_diagnostics() {
        let mut memory = memory_with_kind(
            7,
            Some("daily:v2:1:2026-09-03:scope:overall"),
            mem_data::MemoryKind::DailySummary,
        );
        memory.data.as_mut().unwrap().metadata = Some(
            serde_json::json!({
                "aggregation_mode": "llm",
                "completion_status": "incomplete",
                "generation_id": "g-2",
                "generation_started_at": "2026-09-03T07:06:32Z",
                "generation_consistency": "partial",
                "input_generation_ids": ["g-1", "g-2"],
                "input_without_generation_id_count": 1,
                "failed_path_bucket_digests": ["failed"],
                "fallback_path_bucket_digests": ["fallback"],
                "missing_path_bucket_digests": ["missing"],
                "path_bucket_diagnostics": [{
                    "path_bucket_digest": "missing",
                    "statuses": ["failed", "missing"],
                    "path_bucket_kind": "label",
                    "path_label": "path:/workspace/example"
                }],
                "context_truncated": true,
                "included_source_count": 3,
                "omitted_source_count": 2,
                "last_material_attempt_generation_id": "g-3",
                "last_material_attempt_started_at": "2026-09-03T08:00:00Z",
                "last_material_attempt_status": "failed"
            })
            .to_string(),
        );

        let entry =
            entry_to_summary(memory_list_entry(memory), SummaryKind::Daily).expect("v2 entry");
        assert_eq!(
            entry.input_generation_ids,
            Some(vec!["g-1".into(), "g-2".into()])
        );
        assert_eq!(entry.input_without_generation_id_count, Some(1));
        assert_eq!(entry.failed_path_bucket_digests, vec!["failed"]);
        assert_eq!(entry.fallback_path_bucket_digests, vec!["fallback"]);
        assert_eq!(entry.missing_path_bucket_digests, vec!["missing"]);
        assert_eq!(entry.path_bucket_diagnostics.len(), 1);
        assert_eq!(
            entry.path_bucket_diagnostics[0].path_label.as_deref(),
            Some("path:/workspace/example")
        );
        assert_eq!(entry.context_truncated, Some(true));
        assert_eq!(entry.included_source_count, Some(3));
        assert_eq!(entry.omitted_source_count, Some(2));
        assert_eq!(
            entry.last_material_attempt_generation_id.as_deref(),
            Some("g-3")
        );
        assert_eq!(
            entry.last_material_attempt_status.as_deref(),
            Some("failed")
        );
    }

    #[test]
    fn parse_weekly_external_id() {
        let p = parse_summary_external_id("weekly:1:2026-W21:_all").expect("parse");
        assert_eq!(p.kind, SummaryKind::Weekly);
        assert_eq!(p.period_key.as_deref(), Some("2026-W21"));
        assert_eq!(p.scope_key.as_deref(), Some("_all"));
    }

    #[test]
    fn parse_ownerless_period_external_id_for_display_compatibility() {
        let p = parse_summary_external_id("daily:2026-05-24:_all").expect("parse");
        assert_eq!(p.kind, SummaryKind::Daily);
        assert_eq!(p.period_key.as_deref(), Some("2026-05-24"));
        assert_eq!(p.scope_key.as_deref(), Some("_all"));
    }

    #[test]
    fn parse_monthly_external_id_with_comma_scope() {
        // scope_key can carry `,`-joined labels; only the first `:` splits.
        let p = parse_summary_external_id("monthly:1:2026-05:proj,team").expect("parse");
        assert_eq!(p.kind, SummaryKind::Monthly);
        assert_eq!(p.period_key.as_deref(), Some("2026-05"));
        assert_eq!(p.scope_key.as_deref(), Some("proj,team"));
    }

    #[test]
    fn parse_rejects_malformed_external_ids() {
        // Unknown prefix.
        assert!(parse_summary_external_id("personality:1").is_none());
        // Period prefix without a scope separator.
        assert!(parse_summary_external_id("daily:2026-05-24").is_none());
        // Empty period component.
        assert!(parse_summary_external_id("weekly::_all").is_none());
        // Non-numeric per-thread id is not a valid summary: form.
        assert!(parse_summary_external_id("summary:abc").is_none());
    }

    #[test]
    fn parse_summary_external_id_accepts_group_summary_namespace() {
        let parsed = parse_summary_external_id("thread-group-summary:42").unwrap();
        assert_eq!(parsed.kind, SummaryKind::ThreadGroup);
        assert_eq!(parsed.group_id, Some(42));
        assert_eq!(parsed.thread_id, None);
        assert_eq!(parsed.period_key, None);
    }

    #[test]
    fn group_summary_queries_use_derived_kind_and_keep_legacy_namespace() {
        let request = build_summary_memory_request_for(SummaryKind::ThreadGroup);
        assert_eq!(
            request.external_id_prefix.as_deref(),
            Some("thread-group-summary:")
        );
        assert_eq!(
            request.memory_kinds,
            vec![mem_data::MemoryKind::DerivedSummary as i32]
        );
    }

    #[test]
    fn numeric_eight_with_legacy_group_namespace_remains_a_group_summary() {
        assert_eq!(
            mem_data::MemoryKind::try_from(8).unwrap(),
            mem_data::MemoryKind::DerivedSummary
        );

        let mut memory = memory_with_kind(
            42,
            Some("thread-group-summary:42"),
            mem_data::MemoryKind::DerivedSummary,
        );
        // Existing group rows stored enum value 8 under the old descriptor name.
        memory.data.as_mut().unwrap().memory_kind = 8;

        let summary = entry_to_summary(memory_list_entry(memory), SummaryKind::ThreadGroup)
            .expect("legacy group summary remains readable");

        assert_eq!(summary.kind, SummaryKind::ThreadGroup);
        assert_eq!(summary.group_id, Some(42));
        assert_eq!(
            summary.external_id.as_deref(),
            Some("thread-group-summary:42")
        );
    }

    #[test]
    fn summary_kind_namespaces_do_not_embed_the_owner() {
        assert_eq!(SummaryKind::PerThread.external_id_namespace(), "summary:");
        assert_eq!(SummaryKind::Daily.external_id_namespace(), "daily:");
        assert_eq!(SummaryKind::Weekly.external_id_namespace(), "weekly:");
        assert_eq!(SummaryKind::Monthly.external_id_namespace(), "monthly:");
        assert_eq!(
            SummaryKind::ThreadGroup.external_id_namespace(),
            "thread-group-summary:"
        );
    }

    #[test]
    fn build_find_request_scopes_by_owner_kind_and_namespace_before_paging() {
        let req = ListSummariesRequest {
            kind: SummaryKind::Weekly,
            ..Default::default()
        };
        let r = build_find_request(&req);
        assert_eq!(r.external_id_prefix.as_deref(), Some("weekly:"));
        assert_eq!(r.user_id.unwrap().value, LOOKBACK_USER_ID);
        assert_eq!(
            r.memory_kinds,
            vec![mem_data::MemoryKind::WeeklySummary as i32]
        );
    }

    #[test]
    fn build_find_request_default_kind_uses_thread_group_namespace() {
        let r = build_find_request(&ListSummariesRequest::default());
        assert_eq!(
            r.external_id_prefix.as_deref(),
            Some("thread-group-summary:")
        );
    }

    #[test]
    fn summary_memory_base_request_is_reused_without_list_only_filters() {
        let base = build_summary_memory_request_for(SummaryKind::Weekly);
        let list = build_find_request(&ListSummariesRequest {
            kind: SummaryKind::Weekly,
            limit: Some(10),
            offset: Some(20),
            labels_any: vec!["agent:codex".into()],
            ..Default::default()
        });

        assert_eq!(base.user_id, list.user_id);
        assert_eq!(base.external_id_prefix, list.external_id_prefix);
        assert_eq!(base.memory_kinds, list.memory_kinds);
        assert!(base.thread_filter.is_none());
        assert_eq!(list.limit, Some(10));
        assert_eq!(list.offset, Some(20));
        assert!(list.thread_filter.is_some());
    }

    #[test]
    fn build_count_request_scopes_by_kind_and_namespace() {
        let request = build_count_request(&CountSummariesRequest {
            kind: SummaryKind::Monthly,
        });
        assert_eq!(request.external_id_prefix.as_deref(), Some("monthly:"));
        assert_eq!(
            request.memory_kinds,
            vec![mem_data::MemoryKind::MonthlySummary as i32]
        );
        assert_eq!(request.user_id.map(|id| id.value), Some(LOOKBACK_USER_ID));
    }

    #[test]
    fn resolve_summary_memory_ref_request_deserializes_memory_id_from_json_string() {
        let req: ResolveSummaryMemoryRefRequest =
            serde_json::from_str(r#"{"memory_id":"9007199254740993"}"#).unwrap();
        assert_eq!(req.memory_id, 9_007_199_254_740_993);
    }

    fn memory_with_external_id(memory_id: i64, external_id: Option<&str>) -> mem_data::Memory {
        memory_with_kind(memory_id, external_id, mem_data::MemoryKind::ThreadSummary)
    }

    fn memory_with_kind(
        memory_id: i64,
        external_id: Option<&str>,
        kind: mem_data::MemoryKind,
    ) -> mem_data::Memory {
        mem_data::Memory {
            id: Some(mem_data::MemoryId { value: memory_id }),
            data: Some(mem_data::MemoryData {
                parent_ids: vec![],
                user_id: Some(mem_data::UserId {
                    value: LOOKBACK_USER_ID,
                }),
                content: String::new(),
                content_type: 0,
                params: None,
                metadata: None,
                created_at: 0,
                updated_at: 0,
                role: 0,
                external_id: external_id.map(str::to_string),
                media_object_id: None,
                thread_ids: vec![],
                memory_kind: kind as i32,
            }),
            media: None,
        }
    }

    fn memory_list_entry(memory: mem_data::Memory) -> mem_svc::MemoryListEntry {
        mem_svc::MemoryListEntry {
            memory: Some(memory),
            ..Default::default()
        }
    }

    #[test]
    fn entry_to_summary_rejects_kind_or_namespace_mismatch() {
        let daily = memory_with_kind(
            1,
            Some("daily:1:2026-07-01:_all"),
            mem_data::MemoryKind::DailySummary,
        );
        assert!(entry_to_summary(memory_list_entry(daily), SummaryKind::Daily).is_some());

        let wrong_kind = memory_with_kind(
            2,
            Some("daily:1:2026-07-01:_all"),
            mem_data::MemoryKind::WeeklySummary,
        );
        assert!(entry_to_summary(memory_list_entry(wrong_kind), SummaryKind::Daily).is_none());

        let wrong_namespace = memory_with_kind(
            3,
            Some("weekly:1:2026-W27:_all"),
            mem_data::MemoryKind::DailySummary,
        );
        assert!(entry_to_summary(memory_list_entry(wrong_namespace), SummaryKind::Daily).is_none());

        let malformed = memory_with_kind(
            4,
            Some("daily:not-a-period"),
            mem_data::MemoryKind::DailySummary,
        );
        assert!(entry_to_summary(memory_list_entry(malformed), SummaryKind::Daily).is_none());
    }

    #[test]
    fn entry_to_summary_rejects_reflection_role_and_namespace() {
        let mut reflection_role =
            memory_with_kind(5, Some("summary:42"), mem_data::MemoryKind::ThreadSummary);
        reflection_role.data.as_mut().unwrap().role = mem_data::MessageRole::RoleReflection as i32;
        assert!(
            entry_to_summary(memory_list_entry(reflection_role), SummaryKind::PerThread).is_none()
        );

        let reflection_namespace = memory_with_kind(
            6,
            Some("reflection:42"),
            mem_data::MemoryKind::ThreadSummary,
        );
        assert!(
            entry_to_summary(
                memory_list_entry(reflection_namespace),
                SummaryKind::PerThread
            )
            .is_none()
        );
    }

    #[test]
    fn entry_to_summary_accepts_assistant_thread_summary() {
        let mut summary =
            memory_with_kind(7, Some("summary:42"), mem_data::MemoryKind::ThreadSummary);
        summary.data.as_mut().unwrap().role = mem_data::MessageRole::RoleAssistant as i32;

        let entry = entry_to_summary(memory_list_entry(summary), SummaryKind::PerThread)
            .expect("normal assistant summaries must remain selectable");
        assert_eq!(entry.thread_id, Some(42));
        assert_eq!(entry.kind, SummaryKind::PerThread);
    }

    #[test]
    fn entry_to_summary_exposes_v2_path_identity_and_completion_metadata() {
        let mut summary = memory_with_kind(
            8,
            Some("daily:v2:1:2026-09-03:source-digest:path:path-digest"),
            mem_data::MemoryKind::DailySummary,
        );
        summary.data.as_mut().unwrap().metadata = Some(
            serde_json::json!({
                "aggregation_mode": "llm",
                "completion_status": "complete",
                "generation_id": "20260903070632",
                "generation_started_at": "2026-09-03T07:06:32Z",
                "generation_consistency": "aligned",
                "path_bucket_kind": "label",
                "path_label": "path:/workspace/example"
            })
            .to_string(),
        );

        let entry = entry_to_summary(memory_list_entry(summary), SummaryKind::Daily)
            .expect("complete v2 entry");
        assert!(!entry.legacy);
        assert_eq!(entry.rollup_scope, Some(RollupScope::Path));
        assert_eq!(entry.source_scope_digest.as_deref(), Some("source-digest"));
        assert_eq!(entry.completion_status, Some(CompletionStatus::Complete));
        assert_eq!(
            entry
                .path_bucket
                .as_ref()
                .and_then(|p| p.path_label.as_deref()),
            Some("path:/workspace/example")
        );
    }

    #[test]
    fn entry_to_summary_rejects_v2_identity_when_required_metadata_is_missing() {
        let summary = memory_with_kind(
            9,
            Some("daily:v2:1:2026-09-03:source-digest:overall"),
            mem_data::MemoryKind::DailySummary,
        );
        assert!(entry_to_summary(memory_list_entry(summary), SummaryKind::Daily).is_none());
    }

    #[test]
    fn summary_selection_page_keeps_next_offset_when_raw_rows_are_filtered() {
        let wrong_namespace = memory_with_kind(
            1,
            Some("weekly:1:2026-W27:_all"),
            mem_data::MemoryKind::DailySummary,
        );
        let wrong_kind = memory_with_kind(
            2,
            Some("daily:1:2026-07-01:_all"),
            mem_data::MemoryKind::WeeklySummary,
        );
        let valid = memory_with_kind(
            3,
            Some("daily:1:2026-07-02:_all"),
            mem_data::MemoryKind::DailySummary,
        );
        let page = summary_selection_page_from_raw(
            vec![
                memory_list_entry(wrong_namespace),
                memory_list_entry(wrong_kind),
                memory_list_entry(valid),
            ],
            SummaryKind::Daily,
            2,
            0,
            &[],
            &[],
            true,
        );
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.next_offset, None);
    }

    #[test]
    fn per_thread_server_continuation_uses_raw_rows_not_parsed_entries() {
        // A `limit + 1` RPC page can contain malformed rows that never reach
        // `entry_to_summary`. Its continuation nevertheless advances by the
        // raw server boundary, so later valid rows remain reachable.
        assert_eq!(per_thread_server_next_offset(3, 2, 0), Some(2));
        assert_eq!(per_thread_server_next_offset(2, 2, 0), None);
    }

    #[test]
    fn summary_selection_page_returns_tail_without_next_offset() {
        let valid = memory_with_kind(
            3,
            Some("daily:1:2026-07-02:_all"),
            mem_data::MemoryKind::DailySummary,
        );
        let page = summary_selection_page_from_raw(
            vec![memory_list_entry(valid)],
            SummaryKind::Daily,
            2,
            0,
            &[],
            &[],
            true,
        );
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.next_offset, None);
    }

    #[test]
    fn summary_selection_page_requires_explicit_legacy_opt_in() {
        let legacy = memory_with_kind(
            3,
            Some("daily:1:2026-07-02:_all"),
            mem_data::MemoryKind::DailySummary,
        );
        let shown_by_default = summary_selection_page_from_raw(
            vec![memory_list_entry(legacy.clone())],
            SummaryKind::Daily,
            2,
            0,
            &[],
            &[],
            false,
        );
        assert!(shown_by_default.entries.is_empty());

        let shown = summary_selection_page_from_raw(
            vec![memory_list_entry(legacy)],
            SummaryKind::Daily,
            2,
            0,
            &[],
            &[],
            true,
        );
        assert_eq!(shown.entries.len(), 1);
        assert!(shown.entries[0].legacy);
    }

    #[test]
    fn path_scoped_selection_never_includes_legacy_period_rows() {
        let legacy = memory_with_kind(
            3,
            Some("daily:1:2026-07-02:_all"),
            mem_data::MemoryKind::DailySummary,
        );
        let page = summary_selection_page_from_raw(
            vec![memory_list_entry(legacy)],
            SummaryKind::Daily,
            2,
            0,
            &[RollupScope::Path],
            &[],
            true,
        );
        assert!(page.entries.is_empty());
    }

    #[test]
    fn complete_default_v2_overall_shadows_only_matching_legacy_all_row() {
        let legacy = memory_with_kind(
            3,
            Some("daily:1:2026-07-02:_all"),
            mem_data::MemoryKind::DailySummary,
        );
        let named_legacy = memory_with_kind(
            4,
            Some("daily:1:2026-07-02:project-a"),
            mem_data::MemoryKind::DailySummary,
        );
        let default_scope = source_scope_digest(&[], Some("LABEL_ALL"));
        let mut v2 = memory_with_kind(
            5,
            Some(&format!("daily:v2:1:2026-07-02:{default_scope}:overall")),
            mem_data::MemoryKind::DailySummary,
        );
        v2.data.as_mut().unwrap().metadata = Some(
            serde_json::json!({
                "aggregation_mode": "llm",
                "completion_status": "complete",
                "generation_id": "g-1",
                "generation_started_at": "2026-09-03T00:00:00Z",
                "generation_consistency": "aligned"
            })
            .to_string(),
        );

        let visible = summary_selection_page_from_raw(
            vec![
                memory_list_entry(legacy.clone()),
                memory_list_entry(named_legacy.clone()),
                memory_list_entry(v2.clone()),
            ],
            SummaryKind::Daily,
            10,
            0,
            &[],
            &[],
            false,
        );
        assert_eq!(visible.entries.len(), 1);
        assert!(visible.entries.iter().all(|entry| !entry.legacy));

        let compatibility = summary_selection_page_from_raw(
            vec![
                memory_list_entry(legacy),
                memory_list_entry(named_legacy),
                memory_list_entry(v2),
            ],
            SummaryKind::Daily,
            10,
            0,
            &[],
            &[],
            true,
        );
        assert_eq!(compatibility.entries.len(), 3);
    }

    #[test]
    fn summary_selection_page_uses_processed_raw_count_for_normal_offset() {
        let first = memory_with_kind(
            1,
            Some("daily:1:2026-07-01:_all"),
            mem_data::MemoryKind::DailySummary,
        );
        let second = memory_with_kind(
            2,
            Some("daily:1:2026-07-02:_all"),
            mem_data::MemoryKind::DailySummary,
        );
        let extra = memory_with_kind(
            3,
            Some("daily:1:2026-07-03:_all"),
            mem_data::MemoryKind::DailySummary,
        );
        let page = summary_selection_page_from_raw(
            vec![
                memory_list_entry(first),
                memory_list_entry(second),
                memory_list_entry(extra),
            ],
            SummaryKind::Daily,
            2,
            0,
            &[],
            &[],
            true,
        );
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.next_offset, Some(2));
    }

    #[test]
    fn memory_to_resolved_ref_extracts_per_thread_id() {
        let m = memory_with_external_id(99, Some("summary:42"));
        let r = memory_to_resolved_ref(m).expect("resolved");
        assert_eq!(r.memory_id, 99);
        assert_eq!(r.thread_id, Some(42));
        assert_eq!(r.kind, Some(SummaryKind::PerThread));
        assert_eq!(r.period_key, None);
        assert_eq!(r.scope_key, None);
        assert_eq!(r.external_id.as_deref(), Some("summary:42"));
    }

    #[test]
    fn memory_to_resolved_ref_extracts_daily_period() {
        let m = memory_with_external_id(7, Some("daily:1:2026-05-24:_all"));
        let r = memory_to_resolved_ref(m).expect("resolved");
        assert_eq!(r.memory_id, 7);
        assert_eq!(r.thread_id, None);
        assert_eq!(r.kind, Some(SummaryKind::Daily));
        assert_eq!(r.period_key.as_deref(), Some("2026-05-24"));
        assert_eq!(r.scope_key.as_deref(), Some("_all"));
    }

    #[test]
    fn memory_to_resolved_ref_extracts_monthly_with_scope() {
        let m = memory_with_external_id(11, Some("monthly:1:2026-05:proj"));
        let r = memory_to_resolved_ref(m).expect("resolved");
        assert_eq!(r.kind, Some(SummaryKind::Monthly));
        assert_eq!(r.period_key.as_deref(), Some("2026-05"));
        assert_eq!(r.scope_key.as_deref(), Some("proj"));
    }

    #[test]
    fn memory_to_resolved_ref_unknown_external_id_keeps_raw() {
        // A memory cited from outside the summary namespace (e.g. a raw
        // conversation memory_id slipped into source_memory_ids by the LLM):
        // the frontend should still get the external_id so it can show a
        // tooltip, but no kind/thread coords.
        let m = memory_with_external_id(5, Some("claude_code:session:abc"));
        let r = memory_to_resolved_ref(m).expect("resolved");
        assert_eq!(r.external_id.as_deref(), Some("claude_code:session:abc"));
        assert_eq!(r.kind, None);
        assert_eq!(r.thread_id, None);
        assert_eq!(r.period_key, None);
    }

    #[test]
    fn memory_to_resolved_ref_handles_missing_data() {
        // `Memory.data` can be unset on truncated wire responses; the helper
        // returns None instead of panicking.
        let m = mem_data::Memory {
            id: Some(mem_data::MemoryId { value: 1 }),
            data: None,
            media: None,
        };
        assert!(memory_to_resolved_ref(m).is_none());
    }

    #[test]
    fn build_find_request_maps_date_windows() {
        let req = ListSummariesRequest {
            kind: SummaryKind::Daily,
            limit: Some(10),
            offset: Some(5),
            created_after_ms: Some(1_000),
            created_before_ms: Some(2_000),
            updated_after_ms: Some(3_000),
            updated_before_ms: Some(4_000),
            ..Default::default()
        };
        let r = build_find_request(&req);
        assert_eq!(r.limit, Some(10));
        assert_eq!(r.offset, Some(5));
        assert_eq!(r.created_after, Some(1_000));
        assert_eq!(r.created_before, Some(2_000));
        assert_eq!(r.updated_after, Some(3_000));
        assert_eq!(r.updated_before, Some(4_000));
    }

    #[test]
    fn period_date_window_filters_visible_rows_after_path_counts_are_collected() {
        let req = ListSummariesRequest {
            updated_after_ms: Some(100),
            updated_before_ms: Some(200),
            ..Default::default()
        };
        assert!(summary_matches_updated_window(150, &req));
        assert!(!summary_matches_updated_window(100, &req));
        assert!(!summary_matches_updated_window(200, &req));
        assert!(!summary_matches_updated_window(99, &req));
    }

    #[test]
    fn path_counts_include_children_outside_the_visible_date_window() {
        let req = ListSummariesRequest {
            updated_after_ms: Some(100),
            updated_before_ms: Some(200),
            ..Default::default()
        };
        let entries = annotate_path_summary_counts(vec![
            test_period_summary_entry(RollupScope::Overall, 150),
            test_period_summary_entry(RollupScope::Path, 50),
            test_period_summary_entry(RollupScope::Path, 250),
        ]);
        let visible: Vec<_> = entries
            .into_iter()
            .filter(|entry| summary_matches_updated_window(entry.updated_at_ms, &req))
            .collect();

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].rollup_scope, Some(RollupScope::Overall));
        assert_eq!(visible[0].path_summary_count, Some(2));
    }

    #[test]
    fn build_find_request_applies_all_summary_and_selected_labels() {
        let req = ListSummariesRequest {
            kind: SummaryKind::PerThread,
            labels_any: vec!["summary".into(), "agent:codex".into()],
            ..Default::default()
        };
        let r = build_find_request(&req);
        assert_eq!(r.external_id_prefix.as_deref(), Some("summary:"));
        let filter = r.thread_filter.expect("thread label filter");
        assert_eq!(filter.user_id, Some(LOOKBACK_USER_ID));
        assert_eq!(filter.labels, vec!["summary", "agent:codex"]);
        assert_eq!(
            filter.label_match_mode,
            Some(mem_data::LabelMatchMode::LabelAll as i32)
        );
    }

    #[test]
    fn build_find_request_leaves_period_detail_for_local_external_id_parsing() {
        let request = build_find_request(&ListSummariesRequest {
            kind: SummaryKind::Daily,
            period_key: Some("2026-07-01".into()),
            ..Default::default()
        });

        assert_eq!(request.external_id_prefix.as_deref(), Some("daily:"));
        assert!(request.updated_after.is_none());
        assert!(request.updated_before.is_none());
    }

    #[test]
    fn summary_period_keys_request_scopes_calendar_by_owner_and_kind() {
        let request = summary_period_keys_request(SummaryKind::Daily);

        assert_eq!(request.external_id_prefix.as_deref(), Some("daily:"));
        assert_eq!(
            request.memory_kinds,
            vec![mem_data::MemoryKind::DailySummary as i32]
        );
        assert!(request.updated_after.is_none());
        assert!(request.updated_before.is_none());
    }

    #[test]
    fn summary_filter_excludes_structural_labels() {
        let labels = aggregate_summary_labels([
            vec!["summary", "agent:codex", "date:2026-07-17"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            vec!["summary", "agent:codex", "branch:main", "scope:_all"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            vec!["summary", "weekly_summary", "iso_week:2026-W29"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            vec!["thread_group_summary"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        ]);
        assert_eq!(
            labels,
            vec![
                LabelWithCount {
                    label: "agent:codex".into(),
                    thread_count: 2
                },
                LabelWithCount {
                    label: "branch:main".into(),
                    thread_count: 1
                },
            ]
        );
    }

    #[test]
    fn summary_cooccurring_filter_excludes_selected_labels() {
        let labels = aggregate_summary_cooccurring_labels(
            [vec!["summary", "agent:codex", "branch:main"]
                .into_iter()
                .map(str::to_owned)
                .collect()],
            &["agent:codex".into()],
        );
        assert_eq!(
            labels,
            vec![LabelWithCount {
                label: "branch:main".into(),
                thread_count: 1
            }]
        );
    }

    #[test]
    fn summary_label_aggregation_excludes_threads_without_summary_memory() {
        let threads = vec![
            SummaryThreadLabels {
                id: 10,
                labels: vec!["summary".into(), "agent:codex".into()],
            },
            SummaryThreadLabels {
                id: 20,
                labels: vec!["summary".into(), "branch:stale".into()],
            },
        ];
        let labels = aggregate_summary_labels(labels_for_summary_memories(threads, [10]));
        assert_eq!(
            labels,
            vec![LabelWithCount {
                label: "agent:codex".into(),
                thread_count: 1
            }]
        );
    }

    #[test]
    fn summary_memory_request_uses_kind_without_thread_filter() {
        let request = build_summary_memory_request();

        assert_eq!(request.user_id.map(|id| id.value), Some(LOOKBACK_USER_ID));
        assert_eq!(request.external_id_prefix.as_deref(), Some("summary:"));
        assert!(request.thread_filter.is_none());
    }
}
