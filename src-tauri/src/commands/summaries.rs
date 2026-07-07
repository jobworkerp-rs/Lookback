//! Tauri commands backing the Summaries page.
//!
//! Summaries are stored as memories owned by the synthetic
//! `summary_user_id=100000`. Their `external_id` encodes the summary kind:
//!
//! - per-thread: `summary:<thread_id>`
//! - daily:      `daily:<YYYY-MM-DD>:<scope_key>`
//! - weekly:     `weekly:<YYYY-Www>:<scope_key>`  (ISO 8601 week)
//! - monthly:    `monthly:<YYYY-MM>:<scope_key>`
//!
//! The `content` field is a JSON document with `title` / `context` /
//! `decisions` / `open_questions` / `followups` categories (see
//! `lang-workers/workers/thread-summary/thread-summary-single.yaml` and the
//! `*-work-summary` workflows).

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use tauri::State;
use tokio_stream::StreamExt;

use crate::error::AppResult;
use crate::grpc::proto::llm_memory::data as mem_data;
use crate::grpc::proto::llm_memory::service as mem_svc;
use crate::grpc::proto::llm_memory::service::memory_service_client::MemoryServiceClient;

use super::AppState;

/// Synthetic owner of every summary memory (per-thread and period). Shared
/// with the dispatch input builders in `commands::import`.
pub(crate) const SUMMARY_USER_ID: i64 = 100_000;

/// The summary granularities, distinguished by `external_id` prefix. Each
/// period kind also carries a summary-thread label (`<kind>_summary`) that
/// the frontend uses to scope full-text search to a granularity — search
/// filters cannot match on `external_id`, only on thread labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SummaryKind {
    /// One summary per imported conversation thread (`summary:<thread_id>`).
    #[default]
    PerThread,
    Daily,
    Weekly,
    Monthly,
}

impl SummaryKind {
    /// The `external_id` prefix used for the server-side LIKE prefix filter.
    fn external_id_prefix(self) -> &'static str {
        match self {
            SummaryKind::PerThread => "summary:",
            SummaryKind::Daily => "daily:",
            SummaryKind::Weekly => "weekly:",
            SummaryKind::Monthly => "monthly:",
        }
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
}

/// Parse a summary `external_id` into its kind and components. Returns `None`
/// for shapes that don't match any known prefix so callers fall back to
/// displaying the raw id.
pub(crate) fn parse_summary_external_id(ext: &str) -> Option<ParsedSummaryExternalId> {
    if let Some(thread_id) = super::parse_i64_after_prefix("summary:", Some(ext)) {
        return Some(ParsedSummaryExternalId {
            kind: SummaryKind::PerThread,
            period_key: None,
            scope_key: None,
            thread_id: Some(thread_id),
        });
    }
    for kind in [
        SummaryKind::Daily,
        SummaryKind::Weekly,
        SummaryKind::Monthly,
    ] {
        if let Some(rest) = ext.strip_prefix(kind.external_id_prefix()) {
            // `<period>:<scope_key>`. scope_key may itself carry `,`-joined
            // labels but never a `:`, so splitting on the first `:` is safe.
            let (period_key, scope_key) = rest.split_once(':')?;
            if period_key.is_empty() {
                return None;
            }
            return Some(ParsedSummaryExternalId {
                kind,
                period_key: Some(period_key.to_string()),
                scope_key: Some(scope_key.to_string()),
                thread_id: None,
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
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListSummariesRequest {
    /// Defaults to `PerThread` for backward compatibility with the original
    /// summaries view.
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
}

/// Build the `FindListByCondition` request for a summaries list query. Pure
/// so the wire-shape (prefix selection, date windows) is unit-tested.
fn build_find_request(req: &ListSummariesRequest) -> mem_svc::FindMemoryListRequest {
    mem_svc::FindMemoryListRequest {
        limit: req.limit,
        offset: req.offset,
        roles: vec![],
        user_id: Some(mem_data::UserId {
            value: SUMMARY_USER_ID,
        }),
        thread_id: None,
        updated_after: req.updated_after_ms,
        updated_before: req.updated_before_ms,
        external_id: None,
        content_types: vec![],
        thread_filter: None,
        created_after: req.created_after_ms,
        created_before: req.created_before_ms,
        sort: None,
        external_id_prefix: Some(req.kind.external_id_prefix().to_string()),
    }
}

#[tauri::command]
pub async fn list_summaries(
    state: State<'_, AppState>,
    req: ListSummariesRequest,
) -> AppResult<Vec<SummaryEntry>> {
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);

    let request = build_find_request(&req);
    let mut stream = client.find_list_by_condition(request).await?.into_inner();
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let entry = item?;
        if let Some(s) = entry_to_summary(entry) {
            out.push(s);
        }
    }
    Ok(out)
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
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);

    let request = mem_svc::MemoryCountCondition {
        roles: vec![],
        user_id: Some(mem_data::UserId {
            value: SUMMARY_USER_ID,
        }),
        thread_id: None,
        updated_after: None,
        updated_before: None,
        external_id: None,
        content_types: vec![],
        thread_filter: None,
        created_after: None,
        created_before: None,
        external_id_prefix: Some(req.kind.external_id_prefix().to_string()),
    };

    let resp = client.count_by_condition(request).await?;
    Ok(resp.into_inner().total)
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListSummaryPeriodKeysRequest {
    pub kind: SummaryKind,
    pub updated_after_ms: Option<i64>,
    pub updated_before_ms: Option<i64>,
}

/// Return the sorted, de-duplicated set of `period_key`s that have a summary
/// in the given window. Backs the calendar's existence dots. The window is
/// kept to one month by callers so the unbounded `limit` stays small; only
/// the `external_id` is kept off each entry (the content still crosses the
/// wire, since there is no projection on `FindListByCondition`).
#[tauri::command]
pub async fn list_summary_period_keys(
    state: State<'_, AppState>,
    req: ListSummaryPeriodKeysRequest,
) -> AppResult<Vec<String>> {
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);

    let request = mem_svc::FindMemoryListRequest {
        limit: None,
        offset: None,
        roles: vec![],
        user_id: Some(mem_data::UserId {
            value: SUMMARY_USER_ID,
        }),
        thread_id: None,
        updated_after: req.updated_after_ms,
        updated_before: req.updated_before_ms,
        external_id: None,
        content_types: vec![],
        thread_filter: None,
        created_after: None,
        created_before: None,
        sort: None,
        external_id_prefix: Some(req.kind.external_id_prefix().to_string()),
    };

    let mut stream = client.find_list_by_condition(request).await?.into_inner();
    let mut keys: BTreeSet<String> = BTreeSet::new();
    while let Some(item) = stream.next().await {
        let entry = item?;
        if let Some(ext) = entry
            .memory
            .and_then(|m| m.data)
            .and_then(|d| d.external_id)
            && let Some(parsed) = parse_summary_external_id(&ext)
            && let Some(period_key) = parsed.period_key
        {
            keys.insert(period_key);
        }
    }
    Ok(keys.into_iter().collect())
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
    }
}

fn memory_to_resolved_ref(memory: mem_data::Memory) -> Option<ResolvedSummaryMemoryRef> {
    let memory_id = memory.id?.value;
    let data = memory.data?;
    let coords = summary_coords_from_external_id(data.external_id.as_deref());
    Some(ResolvedSummaryMemoryRef {
        memory_id,
        thread_id: coords.thread_id,
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
/// `SUMMARY_USER_ID`), so deletion is `/llm_memory.service.MemoryService/Delete`
/// by the backing `memory_id` — identical for per-thread / daily / weekly /
/// monthly. The UI gates this behind a confirm dialog.
#[tauri::command]
pub async fn delete_summary(
    state: State<'_, AppState>,
    req: DeleteSummaryRequest,
) -> AppResult<()> {
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);
    client
        .delete(mem_data::MemoryId {
            value: req.memory_id,
        })
        .await?;
    Ok(())
}

fn entry_to_summary(e: mem_svc::MemoryListEntry) -> Option<SummaryEntry> {
    let memory = e.memory?;
    let data = memory.data?;
    let coords = summary_coords_from_external_id(data.external_id.as_deref());
    Some(SummaryEntry {
        memory_id: memory.id?.value,
        thread_id: coords.thread_id,
        external_id: data.external_id,
        kind: coords.kind.unwrap_or_default(),
        period_key: coords.period_key,
        scope_key: coords.scope_key,
        content_json: data.content,
        updated_at_ms: data.updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_summary_request_deserializes_memory_id_from_json_string() {
        let req: DeleteSummaryRequest =
            serde_json::from_str(r#"{"memory_id":"9007199254740993"}"#).unwrap();
        assert_eq!(req.memory_id, 9_007_199_254_740_993);
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
        let p = parse_summary_external_id("daily:2026-05-24:_all").expect("parse");
        assert_eq!(p.kind, SummaryKind::Daily);
        assert_eq!(p.period_key.as_deref(), Some("2026-05-24"));
        assert_eq!(p.scope_key.as_deref(), Some("_all"));
        assert_eq!(p.thread_id, None);
    }

    #[test]
    fn parse_weekly_external_id() {
        let p = parse_summary_external_id("weekly:2026-W21:_all").expect("parse");
        assert_eq!(p.kind, SummaryKind::Weekly);
        assert_eq!(p.period_key.as_deref(), Some("2026-W21"));
        assert_eq!(p.scope_key.as_deref(), Some("_all"));
    }

    #[test]
    fn parse_monthly_external_id_with_comma_scope() {
        // scope_key can carry `,`-joined labels; only the first `:` splits.
        let p = parse_summary_external_id("monthly:2026-05:proj,team").expect("parse");
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
    fn summary_kind_prefixes() {
        assert_eq!(SummaryKind::PerThread.external_id_prefix(), "summary:");
        assert_eq!(SummaryKind::Daily.external_id_prefix(), "daily:");
        assert_eq!(SummaryKind::Weekly.external_id_prefix(), "weekly:");
        assert_eq!(SummaryKind::Monthly.external_id_prefix(), "monthly:");
    }

    #[test]
    fn build_find_request_selects_prefix_by_kind() {
        let req = ListSummariesRequest {
            kind: SummaryKind::Weekly,
            ..Default::default()
        };
        let r = build_find_request(&req);
        assert_eq!(r.external_id_prefix.as_deref(), Some("weekly:"));
        assert_eq!(r.user_id.unwrap().value, SUMMARY_USER_ID);
    }

    #[test]
    fn build_find_request_default_kind_is_per_thread() {
        let r = build_find_request(&ListSummariesRequest::default());
        assert_eq!(r.external_id_prefix.as_deref(), Some("summary:"));
    }

    #[test]
    fn resolve_summary_memory_ref_request_deserializes_memory_id_from_json_string() {
        let req: ResolveSummaryMemoryRefRequest =
            serde_json::from_str(r#"{"memory_id":"9007199254740993"}"#).unwrap();
        assert_eq!(req.memory_id, 9_007_199_254_740_993);
    }

    fn memory_with_external_id(memory_id: i64, external_id: Option<&str>) -> mem_data::Memory {
        mem_data::Memory {
            id: Some(mem_data::MemoryId { value: memory_id }),
            data: Some(mem_data::MemoryData {
                parent_ids: vec![],
                user_id: Some(mem_data::UserId {
                    value: SUMMARY_USER_ID,
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
            }),
            media: None,
        }
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
        let m = memory_with_external_id(7, Some("daily:2026-05-24:_all"));
        let r = memory_to_resolved_ref(m).expect("resolved");
        assert_eq!(r.memory_id, 7);
        assert_eq!(r.thread_id, None);
        assert_eq!(r.kind, Some(SummaryKind::Daily));
        assert_eq!(r.period_key.as_deref(), Some("2026-05-24"));
        assert_eq!(r.scope_key.as_deref(), Some("_all"));
    }

    #[test]
    fn memory_to_resolved_ref_extracts_monthly_with_scope() {
        let m = memory_with_external_id(11, Some("monthly:2026-05:proj"));
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
        };
        let r = build_find_request(&req);
        assert_eq!(r.limit, Some(10));
        assert_eq!(r.offset, Some(5));
        assert_eq!(r.created_after, Some(1_000));
        assert_eq!(r.created_before, Some(2_000));
        assert_eq!(r.updated_after, Some(3_000));
        assert_eq!(r.updated_before, Some(4_000));
    }
}
