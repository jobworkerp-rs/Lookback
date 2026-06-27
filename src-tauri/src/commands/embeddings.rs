//! Tauri commands backing the Settings "Embedding index" card.
//!
//! Targets the **memory** vector index (`MemoryVectorService`), which covers
//! summaries and thread bodies — the data semantic / hybrid / natural-language
//! search runs against. The companion reflection-intent index is handled by
//! `commands::reflections`.
//!
//! `MemoryVectorService.RedispatchEmbeddings` does NOT distinguish "missing
//! only" from "force all"; the server scans every row and re-issues the
//! embedding job idempotently, so the UI surfaces a single "再生成" button
//! instead of two near-identical modes.

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::error::AppResult;
use crate::grpc::proto::llm_memory::service as mem_svc;
use crate::grpc::proto::llm_memory::service::memory_vector_service_client::MemoryVectorServiceClient;

// The reflection side already exposes the wire-identical result struct
// (`dispatched / skipped / failed / duration_ms`). Re-export instead of
// defining a parallel `RedispatchMemoryEmbeddingsResult` so the Tauri layer
// matches the frontend — `src/api/index.ts` already returns a single
// `RedispatchEmbeddingsResult` from both endpoints.
pub use super::reflections::RedispatchEmbeddingsResult;

use super::AppState;

/// Stats projected from `IndexStatsResponse`. Shape parallels
/// `ReflectionIntentIndexStats` so the frontend can share its rendering
/// helpers; the FTS / distance fields are dropped because the card only
/// reports the embedding-coverage view.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MemoryEmbeddingStats {
    pub total_records: u64,
    pub records_with_embedding: u64,
    pub records_without_embedding: u64,
    /// 0 means the vector table is missing (MEMORY_VECTOR_ENABLED unset
    /// or the sidecar is not up).
    pub vector_dimension: u32,
}

fn stats_from_proto(resp: mem_svc::IndexStatsResponse) -> MemoryEmbeddingStats {
    MemoryEmbeddingStats {
        total_records: resp.total_records,
        records_with_embedding: resp.records_with_embedding,
        records_without_embedding: resp.records_without_embedding,
        vector_dimension: resp.vector_dimension,
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RedispatchMemoryEmbeddingsRequest {
    pub user_id: Option<i64>,
    pub thread_id: Option<i64>,
    pub batch_size: Option<u32>,
}

/// Pure builder so tests can pin the wire shape without a sidecar. The
/// proto `kinds` field stays empty: the memory side card scope is "all
/// content kinds", and the empty-kinds default in the server means "no
/// filter".
fn build_redispatch_request(
    req: &RedispatchMemoryEmbeddingsRequest,
) -> mem_svc::RedispatchEmbeddingsRequest {
    mem_svc::RedispatchEmbeddingsRequest {
        user_id: req.user_id,
        thread_id: req.thread_id,
        batch_size: req.batch_size,
        kinds: Vec::new(),
    }
}

#[tauri::command]
pub async fn get_memory_embedding_stats(
    state: State<'_, AppState>,
) -> AppResult<MemoryEmbeddingStats> {
    let mut client = MemoryVectorServiceClient::new(state.memories_channel().await?);
    let resp = client
        .get_index_stats(mem_svc::GetIndexStatsRequest {})
        .await?
        .into_inner();
    Ok(stats_from_proto(resp))
}

#[tauri::command]
pub async fn redispatch_memory_embeddings(
    state: State<'_, AppState>,
    req: RedispatchMemoryEmbeddingsRequest,
) -> AppResult<RedispatchEmbeddingsResult> {
    let request = build_redispatch_request(&req);
    let mut client = MemoryVectorServiceClient::new(state.memories_channel().await?);
    let resp = client.redispatch_embeddings(request).await?.into_inner();
    Ok(RedispatchEmbeddingsResult {
        dispatched_count: resp.dispatched_count,
        skipped_count: resp.skipped_count,
        failed_count: resp.failed_count,
        duration_ms: resp.duration_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_redispatch_request_default_omits_filters() {
        // An empty request must round-trip into a proto request with no
        // filters set so the server scans every memory row. Pins both the
        // None passthrough and the "empty kinds = all" convention.
        let req = RedispatchMemoryEmbeddingsRequest::default();
        let out = build_redispatch_request(&req);
        assert!(out.user_id.is_none());
        assert!(out.thread_id.is_none());
        assert!(out.batch_size.is_none());
        assert!(out.kinds.is_empty());
    }

    #[test]
    fn build_redispatch_request_passes_through_filters() {
        // Keep the door open for a future "scope to a single user / thread"
        // entry point without re-deriving the request shape. The frontend
        // never sets these today, but the field-level passthrough is the
        // contract we want to lock in.
        let req = RedispatchMemoryEmbeddingsRequest {
            user_id: Some(1),
            thread_id: Some(42),
            batch_size: Some(250),
        };
        let out = build_redispatch_request(&req);
        assert_eq!(out.user_id, Some(1));
        assert_eq!(out.thread_id, Some(42));
        assert_eq!(out.batch_size, Some(250));
        assert!(out.kinds.is_empty());
    }

    #[test]
    fn stats_from_proto_projects_only_coverage_fields() {
        // The card only shows the three counters + dimension; the FTS /
        // distance fields from IndexStatsResponse must not leak through the
        // projection (UI has no plumbing for them).
        let resp = mem_svc::IndexStatsResponse {
            total_records: 100,
            records_with_embedding: 80,
            records_without_embedding: 20,
            vector_dimension: 1024,
            distance_type: 0,
            last_optimized_at: 0,
            fts_tokenizer: 0,
            fts_ngram_min: None,
            fts_ngram_max: None,
        };
        let stats = stats_from_proto(resp);
        assert_eq!(stats.total_records, 100);
        assert_eq!(stats.records_with_embedding, 80);
        assert_eq!(stats.records_without_embedding, 20);
        assert_eq!(stats.vector_dimension, 1024);
    }
}
