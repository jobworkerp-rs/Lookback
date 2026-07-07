//! Tauri commands backing the Threads-tab search panel.
//!
//! Three modes are exposed:
//!   - `Keyword`  → `MemoryVectorService.SearchByText` (BM25 only).
//!   - `Semantic` → `MemoryVectorService.SearchSemantic` (server-side
//!     embedding via the Metal `MultimodalEmbeddingRunner`).
//!   - `Hybrid`   → `MemoryVectorService.HybridSearch` (vector + BM25
//!     fusion). Unlike `SearchSemantic`, hybrid requires the *client* to
//!     supply `query_vectors`; we obtain that vector by dispatching the
//!     embedding worker (`jobworkerp::embedding::embed_query`).
//!
//! Results come back at memory granularity and are aggregated into
//! thread-keyed buckets: max-score representative,
//! ties broken by recency, hits without a representative thread
//! (ROLE_SYSTEM rows) skipped.

use serde::{Deserialize, Serialize};
use tauri::State;
use tokio_stream::StreamExt;
use tracing::warn;

use crate::error::AppResult;
use crate::grpc::proto::llm_memory::data as mem_data;
use crate::grpc::proto::llm_memory::service as mem_svc;
use crate::grpc::proto::llm_memory::service::memory_vector_service_client::MemoryVectorServiceClient;

use super::AppState;
use super::threads::LabelMatch;

/// Keyword (BM25), Semantic (server-side embed), or Hybrid (client embed
/// + BM25 fusion) — see module docs.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SearchMode {
    Keyword,
    Semantic,
    Hybrid,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchThreadsRequest {
    pub query_text: String,
    pub mode: SearchMode,
    pub user_id: Option<i64>,
    pub created_after_ms: Option<i64>,
    pub created_before_ms: Option<i64>,
    /// Empty when no label filter is applied; serde rejects missing `Vec`
    /// fields by default, hence the `#[serde(default)]`.
    #[serde(default)]
    pub labels_any: Vec<String>,
    #[serde(default)]
    pub label_match: Option<LabelMatch>,
    /// Memory-side cap, default 50. Aggregated thread count is always
    /// ≤ this number.
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadHit {
    #[serde(with = "crate::serde_id")]
    pub thread_id: i64,
    pub thread_description: Option<String>,
    /// Score of the representative memory (max over the bucket).
    pub top_score: f32,
    /// Id of the representative memory, so the UI can scroll the opened thread
    /// to the hit (the thread fetch returns every memory, not just this one).
    #[serde(with = "crate::serde_id")]
    pub top_memory_id: i64,
    /// Content of the representative memory; raw text for snippet display.
    pub top_snippet: String,
    /// `(position, thread_total)` for "N / M" rendering when both are set.
    pub top_position: Option<i32>,
    pub top_thread_total: Option<i32>,
    pub top_created_at_ms: i64,
    pub hit_count: u32,
}

#[tauri::command]
pub async fn search_memories_keyword(
    state: State<'_, AppState>,
    req: SearchThreadsRequest,
) -> AppResult<Vec<ThreadHit>> {
    let mut client = MemoryVectorServiceClient::new(state.memories_channel().await?);
    let options = Some(build_search_options(&req));
    let request = mem_svc::TextSearchRequest {
        query_text: req.query_text.clone(),
        options,
        fts_options: None,
    };
    let mut stream = client.search_by_text(request).await?.into_inner();
    let mut hits = Vec::new();
    while let Some(item) = stream.next().await {
        hits.push(item?);
    }
    Ok(aggregate_into_threads(hits))
}

#[tauri::command]
pub async fn search_memories_semantic(
    state: State<'_, AppState>,
    req: SearchThreadsRequest,
) -> AppResult<Vec<ThreadHit>> {
    // Server-side embedding of the query; unavailable when the local vector
    // store is degraded (local mode only — remote embeds on the remote side).
    state.ensure_local_embedding_available()?;
    let mut client = MemoryVectorServiceClient::new(state.memories_channel().await?);
    let options = Some(build_search_options(&req));
    let request = mem_svc::SemanticTextSearchRequest {
        query_text: req.query_text.clone(),
        options,
    };
    let mut stream = client.search_semantic(request).await?.into_inner();
    let mut hits = Vec::new();
    while let Some(item) = stream.next().await {
        hits.push(item?);
    }
    Ok(aggregate_into_threads(hits))
}

#[tauri::command]
pub async fn search_memories_hybrid(
    state: State<'_, AppState>,
    req: SearchThreadsRequest,
) -> AppResult<Vec<ThreadHit>> {
    // Client-side embed: hybrid needs `query_vectors` (the server fuses the
    // vector hits with its own BM25 pass over `query_text`). Gated when the
    // local vector store is degraded (local mode only).
    state.ensure_local_embedding_available()?;
    let handle = state.jobworkerp().await?;
    let vector = crate::jobworkerp::embedding::embed_query(&handle, &req.query_text).await?;

    let mut client = MemoryVectorServiceClient::new(state.memories_channel().await?);
    let request = build_hybrid_request(&req, vector);
    let mut stream = client.hybrid_search(request).await?.into_inner();
    let mut hits = Vec::new();
    while let Some(item) = stream.next().await {
        hits.push(item?);
    }
    Ok(aggregate_into_threads(hits))
}

/// Build the `HybridSearchRequest` from the Tauri request plus a single
/// query vector. The embedding worker may return multiple chunks, but
/// hybrid search expects exactly one vector — `embed_query` already returns
/// only the first chunk, so we wrap it as a single `EmbeddingVector`.
fn build_hybrid_request(
    req: &SearchThreadsRequest,
    vector: Vec<f32>,
) -> mem_svc::HybridSearchRequest {
    mem_svc::HybridSearchRequest {
        query_vectors: vec![mem_data::EmbeddingVector { values: vector }],
        query_text: req.query_text.clone(),
        options: Some(build_search_options(req)),
        // Server defaults govern the vector/BM25 weighting; we don't expose
        // tuning in the MVP UI.
        hybrid_options: None,
        fts_options: None,
    }
}

/// Build the `SearchOptions` proto from the Tauri request.
///
/// Search bounds map directly to LanceDB columns:
///   * `created_after`  → strict (`>`)
///   * `created_before` → inclusive (`<=`)
///
/// `user_id` defaults to 1 for single-user local isolation.
/// Label filter lives inside `thread_filter` so it ANDs with the memory
/// owner filter rather than overlapping it.
///
/// Reused for summary search: callers pass `user_id=100000` (the synthetic
/// summary owner) plus `labels_any=[<kind>_summary]` to scope to a
/// granularity. `MemorySearchFilter` has no `external_id` field, so the
/// daily/weekly/monthly distinction can only be made via the summary-thread
/// label — not the `external_id` prefix that list queries use.
fn build_search_options(req: &SearchThreadsRequest) -> mem_data::SearchOptions {
    let user_id = req.user_id.unwrap_or(1);
    let thread_filter = if req.labels_any.is_empty() {
        None
    } else {
        Some(mem_data::ThreadSearchFilter {
            user_id: Some(user_id),
            labels: req.labels_any.clone(),
            label_match_mode: Some(req.label_match.unwrap_or(LabelMatch::Any).to_proto()),
            channel: None,
            created_after: None,
            created_before: None,
            updated_after: None,
            updated_before: None,
        })
    };

    let filter = mem_data::MemorySearchFilter {
        user_id: Some(user_id),
        roles: vec![],
        content_types: vec![],
        created_after: req.created_after_ms,
        created_before: req.created_before_ms,
        updated_after: None,
        updated_before: None,
        thread_filter,
    };
    mem_data::SearchOptions {
        limit: req.limit.unwrap_or(50),
        distance_type: None,
        filter: Some(filter),
        aggregation_strategy: None,
        // Snippet rendering needs the body, so force inclusion even
        // though the server defaults to true today (insulates against
        // a default flip on the memories side).
        include_content: Some(true),
    }
}

/// Collapse memory-level hits into thread cards.
///
/// Rules:
///   * Hits without `thread_id` (ROLE_SYSTEM cross-user rows) are
///     dropped — picking a representative thread would silently route
///     a result to another user's thread.
///   * Within a thread: the highest-scoring memory becomes the
///     representative (snippet / position). Ties broken by latest
///     `created_at`.
///   * Final list sorted by `(top_score desc, top_created_at desc)`.
pub(crate) fn aggregate_into_threads(hits: Vec<mem_svc::MemorySearchResult>) -> Vec<ThreadHit> {
    use std::collections::HashMap;
    let mut by_thread: HashMap<i64, ThreadHit> = HashMap::new();
    for hit in hits {
        let Some(thread_id_msg) = hit.thread_id.as_ref() else {
            // ROLE_SYSTEM / orphaned representative — skip silently;
            // logging at warn so production logs surface unexpected gaps.
            warn!(score = hit.score, "search hit dropped: no thread_id");
            continue;
        };
        let thread_id = thread_id_msg.value;
        let (memory_id, content, created_at_ms) = match memory_excerpt(&hit) {
            Some(t) => t,
            None => {
                warn!(thread_id, "search hit dropped: empty memory payload");
                continue;
            }
        };
        let entry = by_thread.entry(thread_id).or_insert_with(|| ThreadHit {
            thread_id,
            thread_description: hit.thread_description.clone(),
            top_score: f32::NEG_INFINITY,
            top_memory_id: 0,
            top_snippet: String::new(),
            top_position: None,
            top_thread_total: None,
            top_created_at_ms: 0,
            hit_count: 0,
        });
        entry.hit_count += 1;
        let should_replace = hit.score > entry.top_score
            || (hit.score == entry.top_score && created_at_ms > entry.top_created_at_ms);
        if should_replace {
            entry.top_score = hit.score;
            entry.top_memory_id = memory_id;
            entry.top_snippet = content;
            entry.top_position = hit.position;
            entry.top_thread_total = hit.thread_total;
            entry.top_created_at_ms = created_at_ms;
            if entry.thread_description.is_none() {
                entry.thread_description = hit.thread_description;
            }
        }
    }
    let mut out: Vec<ThreadHit> = by_thread.into_values().collect();
    out.sort_by(|a, b| {
        b.top_score
            .partial_cmp(&a.top_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.top_created_at_ms.cmp(&a.top_created_at_ms))
    });
    out
}

fn memory_excerpt(hit: &mem_svc::MemorySearchResult) -> Option<(i64, String, i64)> {
    let memory = hit.memory.as_ref()?;
    let memory_id = memory.id.as_ref()?.value;
    let data = memory.data.as_ref()?;
    Some((memory_id, data.content.clone(), data.created_at))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(
        thread_id: Option<i64>,
        score: f32,
        content: &str,
        created_at: i64,
        position: Option<i32>,
    ) -> mem_svc::MemorySearchResult {
        hit_with_id(1, thread_id, score, content, created_at, position)
    }

    #[allow(clippy::too_many_arguments)]
    fn hit_with_id(
        memory_id: i64,
        thread_id: Option<i64>,
        score: f32,
        content: &str,
        created_at: i64,
        position: Option<i32>,
    ) -> mem_svc::MemorySearchResult {
        mem_svc::MemorySearchResult {
            memory: Some(mem_data::Memory {
                id: Some(mem_data::MemoryId { value: memory_id }),
                data: Some(mem_data::MemoryData {
                    content: content.into(),
                    created_at,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            score,
            distance: 0.0,
            score_source: 0,
            position,
            thread_total: position.map(|p| p + 10),
            thread_id: thread_id.map(|v| mem_data::ThreadId { value: v }),
            thread_owner_user_id: Some(mem_data::UserId { value: 1 }),
            thread_description: Some("Thread T".into()),
            highlights: vec![],
            matched_vector_kind: None,
            matched_begin_position: None,
            matched_end_position: None,
            matched_content: None,
        }
    }

    #[test]
    fn build_search_options_uses_user_id_1_by_default() {
        let req = SearchThreadsRequest {
            query_text: "x".into(),
            mode: SearchMode::Keyword,
            user_id: None,
            created_after_ms: None,
            created_before_ms: None,
            labels_any: vec![],
            label_match: None,
            limit: None,
        };
        let opts = build_search_options(&req);
        assert_eq!(opts.filter.unwrap().user_id, Some(1));
    }

    #[test]
    fn build_search_options_maps_dates_and_default_limit() {
        let req = SearchThreadsRequest {
            query_text: "x".into(),
            mode: SearchMode::Keyword,
            user_id: Some(7),
            created_after_ms: Some(1_000),
            created_before_ms: Some(2_000),
            labels_any: vec![],
            label_match: None,
            limit: None,
        };
        let opts = build_search_options(&req);
        assert_eq!(opts.limit, 50);
        assert_eq!(opts.include_content, Some(true));
        let filter = opts.filter.unwrap();
        assert_eq!(filter.user_id, Some(7));
        assert_eq!(filter.created_after, Some(1_000));
        assert_eq!(filter.created_before, Some(2_000));
    }

    #[test]
    fn build_search_options_passes_labels_into_thread_filter_with_any_mode() {
        let req = SearchThreadsRequest {
            query_text: "x".into(),
            mode: SearchMode::Keyword,
            user_id: None,
            created_after_ms: None,
            created_before_ms: None,
            labels_any: vec!["lookback".into(), "review".into()],
            label_match: None,
            limit: Some(100),
        };
        let opts = build_search_options(&req);
        assert_eq!(opts.limit, 100);
        let tf = opts.filter.unwrap().thread_filter.expect("thread_filter");
        assert_eq!(
            tf.labels,
            vec!["lookback".to_string(), "review".to_string()]
        );
        // ANY = 0 per llm_memory.data.LabelMatchMode (common.proto).
        assert_eq!(
            tf.label_match_mode,
            Some(mem_data::LabelMatchMode::LabelAny as i32)
        );
        assert_eq!(tf.user_id, Some(1));
    }

    #[test]
    fn build_search_options_passes_labels_with_all_mode_when_requested() {
        // The Threads-tab AND/OR toggle drives this via `label_match`.
        let req = SearchThreadsRequest {
            query_text: "x".into(),
            mode: SearchMode::Keyword,
            user_id: None,
            created_after_ms: None,
            created_before_ms: None,
            labels_any: vec!["lookback".into(), "review".into()],
            label_match: Some(LabelMatch::All),
            limit: None,
        };
        let opts = build_search_options(&req);
        let tf = opts.filter.unwrap().thread_filter.expect("thread_filter");
        assert_eq!(
            tf.label_match_mode,
            Some(mem_data::LabelMatchMode::LabelAll as i32)
        );
    }

    #[test]
    fn aggregate_groups_by_thread_id_keeps_max_score() {
        // Three hits on thread 10, one on thread 20. Thread 10's top
        // memory becomes the representative; hit_count tallies all
        // memberships.
        let hits = vec![
            hit_with_id(101, Some(10), 0.5, "low", 100, Some(1)),
            hit_with_id(102, Some(10), 0.9, "top", 200, Some(2)),
            hit_with_id(103, Some(10), 0.7, "mid", 150, Some(3)),
            hit_with_id(201, Some(20), 0.6, "other", 100, Some(1)),
        ];
        let mut out = aggregate_into_threads(hits);
        // Sorted by top_score desc; thread 10 (0.9) before thread 20 (0.6).
        assert_eq!(out.len(), 2);
        let t10 = out.remove(0);
        assert_eq!(t10.thread_id, 10);
        assert_eq!(t10.top_snippet, "top");
        assert_eq!(t10.top_memory_id, 102); // the 0.9 hit, not 101/103
        assert_eq!(t10.top_position, Some(2));
        assert_eq!(t10.top_thread_total, Some(12));
        assert_eq!(t10.hit_count, 3);
        let t20 = out.remove(0);
        assert_eq!(t20.thread_id, 20);
        assert_eq!(t20.top_snippet, "other");
        assert_eq!(t20.top_memory_id, 201);
    }

    #[test]
    fn aggregate_breaks_score_ties_by_recency() {
        // Same score on thread 1: the later `created_at` becomes
        // representative.
        let hits = vec![
            hit_with_id(11, Some(1), 0.8, "older", 100, Some(1)),
            hit_with_id(12, Some(1), 0.8, "newer", 200, Some(5)),
        ];
        let out = aggregate_into_threads(hits);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].top_snippet, "newer");
        assert_eq!(out[0].top_memory_id, 12); // tie broken to the newer hit
        assert_eq!(out[0].top_position, Some(5));
        assert_eq!(out[0].top_created_at_ms, 200);
        assert_eq!(out[0].hit_count, 2);
    }

    #[test]
    fn aggregate_skips_results_without_thread_id() {
        // ROLE_SYSTEM hits arrive without a representative thread —
        // they must not collide on `thread_id == 0` and pollute a
        // real bucket.
        let hits = vec![
            hit(None, 0.95, "system prompt", 100, None),
            hit(Some(10), 0.3, "thread member", 200, Some(1)),
        ];
        let out = aggregate_into_threads(hits);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].thread_id, 10);
    }

    #[test]
    fn aggregate_sorts_buckets_by_score_then_recency() {
        // Two threads with identical top scores: newer one comes first.
        let hits = vec![
            hit(Some(1), 0.9, "a", 100, Some(1)),
            hit(Some(2), 0.9, "b", 200, Some(1)),
        ];
        let out = aggregate_into_threads(hits);
        assert_eq!(out[0].thread_id, 2); // newer wins outer tie-break
        assert_eq!(out[1].thread_id, 1);
    }

    #[test]
    fn aggregate_skips_hits_missing_memory_payload() {
        // Defensive: malformed hit (no Memory message) doesn't panic.
        let mut h = hit(Some(5), 0.5, "x", 100, Some(1));
        h.memory = None;
        let out = aggregate_into_threads(vec![h]);
        assert!(out.is_empty());
    }

    #[test]
    fn build_hybrid_request_wraps_single_vector_and_forwards_query() {
        let req = SearchThreadsRequest {
            query_text: "rust ownership".into(),
            mode: SearchMode::Hybrid,
            user_id: Some(3),
            created_after_ms: Some(1_000),
            created_before_ms: None,
            labels_any: vec![],
            label_match: None,
            limit: Some(25),
        };
        let r = build_hybrid_request(&req, vec![0.1, 0.2, 0.3]);
        assert_eq!(r.query_text, "rust ownership");
        assert_eq!(r.query_vectors.len(), 1);
        assert_eq!(r.query_vectors[0].values, vec![0.1, 0.2, 0.3]);
        assert!(r.hybrid_options.is_none());
        assert!(r.fts_options.is_none());
        let opts = r.options.expect("options always set");
        assert_eq!(opts.limit, 25);
        let filter = opts.filter.unwrap();
        assert_eq!(filter.user_id, Some(3));
        assert_eq!(filter.created_after, Some(1_000));
    }

    #[test]
    fn build_hybrid_request_carries_label_filter() {
        let req = SearchThreadsRequest {
            query_text: "x".into(),
            mode: SearchMode::Hybrid,
            user_id: None,
            created_after_ms: None,
            created_before_ms: None,
            labels_any: vec!["lookback".into()],
            label_match: None,
            limit: None,
        };
        let r = build_hybrid_request(&req, vec![1.0]);
        let tf = r.options.unwrap().filter.unwrap().thread_filter.unwrap();
        assert_eq!(tf.labels, vec!["lookback".to_string()]);
    }

    #[test]
    fn aggregate_keeps_thread_description_from_first_seen_when_top_loses() {
        // First hit has description but lower score; the eventual top
        // hit has no description. The bucket should still carry the
        // earlier description so the UI has a thread name to show.
        let mut h1 = hit(Some(7), 0.4, "first", 100, Some(1));
        h1.thread_description = Some("Real name".into());
        let mut h2 = hit(Some(7), 0.9, "top", 200, Some(2));
        h2.thread_description = None;
        let out = aggregate_into_threads(vec![h1, h2]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].top_snippet, "top");
        assert_eq!(out[0].thread_description.as_deref(), Some("Real name"));
    }
}
