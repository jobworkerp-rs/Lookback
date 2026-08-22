//! Tauri commands backing the Reflections tab.
//!
//! Reflections live in a dedicated entity (not user_id-owned memories) so
//! we route through `ReflectionService` instead of the `MemoryService`
//! path used for summaries/personality. `FindByThread` returns the
//! per-thread reflection stream; `Search` covers cross-thread listing
//! with optional filters.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tauri::State;
use tokio_stream::StreamExt;

use crate::error::{AppError, AppResult};
use crate::grpc::proto::llm_memory::data as mem_data;
use crate::grpc::proto::llm_memory::service as mem_svc;
use crate::grpc::proto::llm_memory::service::memory_vector_service_client::MemoryVectorServiceClient;
use crate::grpc::proto::llm_memory::service::reflection_service_client::ReflectionServiceClient;
use crate::grpc::proto::llm_memory::service::reflection_vector_service_client::ReflectionVectorServiceClient;

use super::AppState;
use super::search::{
    SearchMode, SearchThreadsRequest, build_hybrid_request as build_thread_hybrid_request,
};

const REFLECTION_MEMORY_USER_ID: i64 = 1;

/// Projected reflection record. Reflection proto has ~40 fields, of which
/// only the ones used by the Reflections tab are forwarded. Frontend maps
/// `task_category` / `reflection_aspect` / `outcome` / `failure_modes` enum
/// values to human-readable labels via `lib/searchTaxonomy.ts`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReflectionEntry {
    #[serde(with = "crate::serde_id")]
    pub id: i64,
    #[serde(with = "crate::serde_id")]
    pub origin_thread_id: i64,
    pub summary: String,
    pub task_intent: String,
    pub task_category: i32,
    pub reflection_aspect: i32,
    pub outcome: i32,
    pub score: f32,
    pub score_self: f32,
    pub score_heuristic: f32,
    pub lessons: Vec<String>,
    pub key_decisions: Vec<String>,
    pub success_factors: Vec<String>,
    pub failure_modes: Vec<i32>,
    pub mitigation_hint: Option<String>,
    pub pinned: bool,
    pub prompt_version: String,
    /// Active judgment relies on `intent_embedding_status == OK`.
    pub intent_embedding_status: i32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Stable, compact projection used by the chat "select material" modal.
/// Reflection IDs are the backing memory IDs by contract, so the frontend
/// can use the same selection snapshot shape as summary memories.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReflectionSelectionEntry {
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
    #[serde(with = "crate::serde_id::option")]
    pub origin_thread_id: Option<i64>,
    pub summary: String,
    pub content_json: String,
    pub source_thread_ids: Vec<String>,
    pub source_memory_ids: Vec<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReflectionSelectionPage {
    pub entries: Vec<ReflectionSelectionEntry>,
    #[serde(with = "crate::serde_id::option")]
    pub next_cursor_after_memory_id: Option<i64>,
}

/// Authoritative full content for one reflection selection. The ACL is
/// applied by `get_reflection_selection_content` before this DTO is built.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReflectionSelectionContent {
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
    #[serde(with = "crate::serde_id::option")]
    pub origin_thread_id: Option<i64>,
    pub content_json: String,
    pub source_thread_ids: Vec<String>,
    pub source_memory_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListReflectionsForSelectionRequest {
    pub limit: Option<u32>,
    #[serde(default, with = "crate::serde_id::option")]
    pub cursor_after_memory_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetReflectionSelectionContentRequest {
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListReflectionsByThreadRequest {
    #[serde(with = "crate::serde_id")]
    pub thread_id: i64,
    /// false = active only (latest by created_at); true = full history.
    /// Maps directly to `FindReflectionsByThreadIdRequest.include_history`.
    #[serde(default)]
    pub include_history: bool,
}

#[tauri::command]
pub async fn list_reflections_by_thread(
    state: State<'_, AppState>,
    req: ListReflectionsByThreadRequest,
) -> AppResult<Vec<ReflectionEntry>> {
    let mut client = ReflectionServiceClient::new(state.memories_channel().await?);

    let request = mem_svc::FindReflectionsByThreadIdRequest {
        thread_id: Some(mem_data::ThreadId {
            value: req.thread_id,
        }),
        include_history: req.include_history,
    };

    let mut stream = client.find_by_thread(request).await?.into_inner();
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        if let Some(entry) = entry_from_proto(item?) {
            out.push(entry);
        }
    }
    Ok(out)
}

/// List reflections for the selection modal using the server's filter-only
/// branch. The cursor is a memory-id keyset cursor, so rows added while the
/// modal is open cannot shift an offset and cause duplicates.
#[tauri::command]
pub async fn list_reflections_for_selection(
    state: State<'_, AppState>,
    req: ListReflectionsForSelectionRequest,
) -> AppResult<ReflectionSelectionPage> {
    let mut client = ReflectionServiceClient::new(state.memories_channel().await?);
    let page_limit = req.limit.unwrap_or(50).max(1);
    let mut search_request = build_selection_list_request(&req);
    // Fetch one extra raw row.  Filtering malformed/foreign rows locally
    // must not make the frontend lose the cursor for valid rows that follow.
    search_request.limit = Some(page_limit.saturating_add(1));
    let response = client.search(search_request).await?.into_inner();
    let mut raw_rows = Vec::new();
    let mut stream = response;
    while let Some(item) = stream.next().await {
        let reflection = item?.reflection;
        let memory_id = reflection
            .as_ref()
            .and_then(|reflection| reflection.id.as_ref())
            .map(|id| id.value);
        raw_rows.push(RawReflectionRow {
            memory_id,
            reflection,
        });
    }
    selection_page_from_raw(raw_rows, page_limit)
}

/// Fetch complete reflection content and provenance for one selected row.
/// `Find` is an ID lookup and does not inherit the list filter, so the
/// origin-user check is mandatory before returning any content.
#[tauri::command]
pub async fn get_reflection_selection_content(
    state: State<'_, AppState>,
    req: GetReflectionSelectionContentRequest,
) -> AppResult<Option<ReflectionSelectionContent>> {
    let mut client = ReflectionServiceClient::new(state.memories_channel().await?);
    let response = client
        .find(mem_svc::FindReflectionRequest {
            id: Some(mem_data::ReflectionId {
                value: req.memory_id,
            }),
        })
        .await?
        .into_inner();
    Ok(response.reflection.and_then(|reflection| {
        reflection_selection_content_for_owner(reflection, REFLECTION_MEMORY_USER_ID)
    }))
}

fn build_selection_list_request(
    req: &ListReflectionsForSelectionRequest,
) -> mem_svc::SearchReflectionsRequest {
    mem_svc::SearchReflectionsRequest {
        query_text: None,
        query_vectors: vec![],
        filter: Some(build_reflection_filter(
            Some(REFLECTION_MEMORY_USER_ID),
            &[],
            None,
            None,
        )),
        sort: None,
        boost_pinned_high_score: None,
        hybrid_options: None,
        limit: req.limit,
        cursor_after_memory_id: req.cursor_after_memory_id,
    }
}

#[derive(Debug)]
struct RawReflectionRow {
    memory_id: Option<i64>,
    reflection: Option<mem_data::Reflection>,
}

fn selection_page_from_raw(
    rows: Vec<RawReflectionRow>,
    page_limit: u32,
) -> AppResult<ReflectionSelectionPage> {
    let limit = page_limit.max(1) as usize;
    let has_more = rows.len() > limit;
    let next_cursor_after_memory_id = if has_more {
        rows.iter()
            .take(limit)
            .rev()
            .find_map(|row| row.memory_id)
            .ok_or_else(|| {
                AppError::Grpc(tonic::Status::data_loss(
                    "reflection search page has no memory id before its boundary",
                ))
            })
            .map(Some)?
    } else {
        None
    };
    let entries = rows
        .into_iter()
        .take(limit)
        .filter_map(|row| row.reflection.and_then(selection_entry_from_proto))
        .collect();
    Ok(ReflectionSelectionPage {
        entries,
        next_cursor_after_memory_id,
    })
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SearchReflectionsRequest {
    /// Empty string = "filter-only listing" per reflection.proto:121-126.
    /// Non-empty triggers BM25 / hybrid ranking via ReflectionService.Search.
    pub query_text: Option<String>,
    pub user_id: Option<i64>,
    /// Outcome OR-list (empty = any).
    #[serde(default)]
    pub outcomes: Vec<i32>,
    pub created_after_ms: Option<i64>,
    pub created_before_ms: Option<i64>,
    pub limit: Option<u32>,
}

#[tauri::command]
pub async fn search_reflections(
    state: State<'_, AppState>,
    req: SearchReflectionsRequest,
) -> AppResult<Vec<ReflectionEntry>> {
    let mut client = ReflectionServiceClient::new(state.memories_channel().await?);

    let request = build_search_request(&req);
    let stream = client.search(request).await?.into_inner();
    collect_reflection_entries(stream).await
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SearchReflectionsHybridRequest {
    /// Natural-language query searched against reflection backing memories.
    pub query_text: String,
    /// Backing memory owner, not origin_user_id. Defaults to the single local user.
    pub user_id: Option<i64>,
    /// Outcome OR-list. Applied after ReflectionService hydration because
    /// MemoryVectorService cannot filter reflection-specific columns.
    #[serde(default)]
    pub outcomes: Vec<i32>,
    pub created_after_ms: Option<i64>,
    pub created_before_ms: Option<i64>,
    pub limit: Option<u32>,
}

#[tauri::command]
pub async fn search_reflections_hybrid(
    state: State<'_, AppState>,
    req: SearchReflectionsHybridRequest,
) -> AppResult<Vec<ReflectionEntry>> {
    state.ensure_local_embedding_available()?;
    let handle = state.jobworkerp().await?;
    let vector = crate::jobworkerp::embedding::embed_query(&handle, &req.query_text).await?;
    let request = build_hybrid_search_request(&req, vector);

    let mut memory_client = MemoryVectorServiceClient::new(state.memories_channel().await?);
    let mut stream = memory_client.hybrid_search(request).await?.into_inner();
    let mut hits = Vec::new();
    while let Some(item) = stream.next().await {
        hits.push(item?);
    }

    let ids = reflection_memory_ids_from_hits(hits);
    let mut reflection_client = ReflectionServiceClient::new(state.memories_channel().await?);
    let mut out = Vec::new();
    for id in ids {
        let resp = reflection_client
            .find(mem_svc::FindReflectionRequest {
                id: Some(mem_data::ReflectionId { value: id }),
            })
            .await?
            .into_inner();
        if let Some(refl) = resp.reflection
            && let Some(entry) = entry_from_proto(refl)
            && (req.outcomes.is_empty() || req.outcomes.contains(&entry.outcome))
        {
            out.push(entry);
        }
    }
    Ok(out)
}

/// Drain a `ReflectionSearchResult` stream into UI DTOs. Both `Search` and
/// `FindSimilarByIntentText` return this shape (the reflection is inline),
/// so the projection lives in one place. Hits with no reflection or an
/// empty summary are dropped by `entry_from_proto`.
async fn collect_reflection_entries(
    mut stream: tonic::Streaming<mem_data::ReflectionSearchResult>,
) -> AppResult<Vec<ReflectionEntry>> {
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        if let Some(refl) = item?.reflection
            && let Some(entry) = entry_from_proto(refl)
        {
            out.push(entry);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SearchReflectionsByIntentRequest {
    /// Natural-language intent query. The server embeds this itself via the
    /// shared embedding worker, so no client-side vector is required.
    pub intent_text: String,
    /// Top-K nearest reflections. Defaults to 20.
    pub top_k: Option<u32>,
    pub user_id: Option<i64>,
    #[serde(default)]
    pub outcomes: Vec<i32>,
    pub created_after_ms: Option<i64>,
    pub created_before_ms: Option<i64>,
}

#[tauri::command]
pub async fn search_reflections_by_intent(
    state: State<'_, AppState>,
    req: SearchReflectionsByIntentRequest,
) -> AppResult<Vec<ReflectionEntry>> {
    // Server-side intent-text embedding against the reflection-intent vector
    // index; unavailable when the local vector store is degraded (local
    // mode only). Structured `search_reflections` stays available.
    state.ensure_local_embedding_available()?;
    let request = build_intent_request(&req)?;
    let mut client = ReflectionServiceClient::new(state.memories_channel().await?);
    let stream = client
        .find_similar_by_intent_text(request)
        .await?
        .into_inner();
    collect_reflection_entries(stream).await
}

/// Build the `FindSimilarByTextRequest`. Empty/whitespace intent is rejected
/// up front so the server isn't asked to embed "".
fn build_intent_request(
    req: &SearchReflectionsByIntentRequest,
) -> AppResult<mem_svc::FindSimilarByTextRequest> {
    let intent = req.intent_text.trim();
    if intent.is_empty() {
        return Err(AppError::Config("intent_text must be non-empty".into()));
    }
    Ok(mem_svc::FindSimilarByTextRequest {
        intent_text: intent.to_string(),
        top_k: req.top_k.unwrap_or(20),
        filter: Some(build_reflection_filter(
            req.user_id,
            &req.outcomes,
            req.created_after_ms,
            req.created_before_ms,
        )),
    })
}

/// Build the shared `ReflectionSearchFilter` (origin_user_id defaults to 1,
/// outcome OR-list, created-at bounds). Reused by both `Search` and
/// `FindSimilarByIntentText` so the filter contract stays in one place.
fn build_reflection_filter(
    user_id: Option<i64>,
    outcomes: &[i32],
    created_after_ms: Option<i64>,
    created_before_ms: Option<i64>,
) -> mem_data::ReflectionSearchFilter {
    mem_data::ReflectionSearchFilter {
        origin_user_id: Some(mem_data::UserId {
            value: user_id.unwrap_or(1),
        }),
        outcomes: outcomes.to_vec(),
        created_after: created_after_ms,
        created_before: created_before_ms,
        ..Default::default()
    }
}

/// Intent-vector index health for the Reflections natural-language search.
/// `records_without_embedding > 0` means some reflections won't surface in
/// intent search yet — either pre-dating the C-3b auto-embedding wiring or a
/// failed auto-dispatch. Surfaced in Settings so the user can confirm the
/// auto-embedding is actually running and decide whether a backfill is needed.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReflectionIntentIndexStats {
    pub total_records: u64,
    pub records_with_embedding: u64,
    pub records_without_embedding: u64,
    /// 0 signals the intent vector table hasn't been created yet
    /// (`REFLECTION_INTENT_VECTOR_ENABLED` unset, or sidecar not up).
    pub vector_dimension: u32,
}

fn stats_from_proto(resp: mem_svc::IndexStatsResponse) -> ReflectionIntentIndexStats {
    ReflectionIntentIndexStats {
        total_records: resp.total_records,
        records_with_embedding: resp.records_with_embedding,
        records_without_embedding: resp.records_without_embedding,
        vector_dimension: resp.vector_dimension,
    }
}

#[tauri::command]
pub async fn get_reflection_intent_index_stats(
    state: State<'_, AppState>,
) -> AppResult<ReflectionIntentIndexStats> {
    let mut client = ReflectionVectorServiceClient::new(state.memories_channel().await?);
    let resp = client
        .get_intent_index_stats(mem_svc::GetIndexStatsRequest {})
        .await?
        .into_inner();
    Ok(stats_from_proto(resp))
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RedispatchReflectionEmbeddingsRequest {
    /// EmbeddingKind selector: INTENT(2) by default (the natural-language
    /// search reads the intent table), BOTH(3) also allowed. UNSPECIFIED(0)
    /// is rejected before the round-trip.
    pub kind: Option<i32>,
    pub user_id: Option<i64>,
    #[serde(default)]
    pub outcomes: Vec<i32>,
    pub created_after_ms: Option<i64>,
    pub created_before_ms: Option<i64>,
    pub batch_size: Option<u32>,
}

/// Shared by `redispatch_reflection_embeddings` and
/// `redispatch_memory_embeddings`: both proto responses carry the same four
/// counters, and the frontend already exposes one TS interface for both.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RedispatchEmbeddingsResult {
    pub dispatched_count: u32,
    pub skipped_count: u32,
    pub failed_count: u32,
    pub duration_ms: i64,
}

/// Build the `RedispatchReflectionEmbeddingsRequest`. Defaults `kind` to
/// INTENT and rejects UNSPECIFIED up front (the server rejects it too, but a
/// local guard avoids a pointless round-trip and gives a clearer error).
/// Reuses `build_reflection_filter` so the redispatch scope matches what the
/// Reflections tab filters on.
fn build_redispatch_request(
    req: &RedispatchReflectionEmbeddingsRequest,
) -> AppResult<mem_svc::RedispatchReflectionEmbeddingsRequest> {
    const EMBEDDING_KIND_UNSPECIFIED: i32 = 0;
    const EMBEDDING_KIND_INTENT: i32 = 2;
    let kind = req.kind.unwrap_or(EMBEDDING_KIND_INTENT);
    if kind == EMBEDDING_KIND_UNSPECIFIED {
        return Err(AppError::Config(
            "embedding kind must not be UNSPECIFIED".into(),
        ));
    }
    Ok(mem_svc::RedispatchReflectionEmbeddingsRequest {
        kind,
        filter: Some(build_reflection_filter(
            req.user_id,
            &req.outcomes,
            req.created_after_ms,
            req.created_before_ms,
        )),
        batch_size: req.batch_size,
    })
}

#[tauri::command]
pub async fn redispatch_reflection_embeddings(
    state: State<'_, AppState>,
    req: RedispatchReflectionEmbeddingsRequest,
) -> AppResult<RedispatchEmbeddingsResult> {
    let request = build_redispatch_request(&req)?;
    let registered = super::begin_search_index_write(
        &state,
        &[crate::search_index_maintenance::MaintenanceTable::Thread],
    )
    .await?;
    let mut client = ReflectionVectorServiceClient::new(state.memories_channel().await?);
    let result = client
        .redispatch_reflection_embeddings(request)
        .await
        .map(|response| response.into_inner());
    let finish = super::finish_search_index_write(&state, registered).await;
    let resp = result?;
    finish?;
    Ok(RedispatchEmbeddingsResult {
        dispatched_count: resp.dispatched_count,
        skipped_count: resp.skipped_count,
        failed_count: resp.failed_count,
        duration_ms: resp.duration_ms,
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteReflectionRequest {
    #[serde(with = "crate::serde_id")]
    pub id: i64,
}

/// Delete a single reflection (`/llm_memory.service.ReflectionService/Delete`).
/// Reflections are not plain Memory rows, so they use the dedicated RPC keyed
/// by `ReflectionId`. The UI gates this behind a confirm dialog.
#[tauri::command]
pub async fn delete_reflection(
    state: State<'_, AppState>,
    req: DeleteReflectionRequest,
) -> AppResult<()> {
    let registered = super::begin_search_index_write(
        &state,
        &[crate::search_index_maintenance::MaintenanceTable::Thread],
    )
    .await?;
    let mut client = ReflectionServiceClient::new(state.memories_channel().await?);
    let result = client
        .delete(mem_svc::DeleteReflectionRequest {
            id: Some(mem_data::ReflectionId { value: req.id }),
        })
        .await
        .map(|_| ());
    let finish = super::finish_search_index_write(&state, registered).await;
    result?;
    finish
}

fn build_search_request(req: &SearchReflectionsRequest) -> mem_svc::SearchReflectionsRequest {
    let filter = build_reflection_filter(
        req.user_id,
        &req.outcomes,
        req.created_after_ms,
        req.created_before_ms,
    );
    mem_svc::SearchReflectionsRequest {
        // Empty query collapses to filter-only listing per the proto
        // contract — we forward `None` rather than `Some("")` so the
        // server applies its filter-only branch.
        query_text: req.query_text.as_deref().and_then(|q| {
            let trimmed = q.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }),
        query_vectors: vec![],
        filter: Some(filter),
        sort: None,
        boost_pinned_high_score: None,
        hybrid_options: None,
        limit: req.limit,
        cursor_after_memory_id: None,
    }
}

fn build_hybrid_search_request(
    req: &SearchReflectionsHybridRequest,
    vector: Vec<f32>,
) -> mem_svc::HybridSearchRequest {
    let thread_req = SearchThreadsRequest {
        query_text: req.query_text.clone(),
        mode: SearchMode::Hybrid,
        user_id: Some(req.user_id.unwrap_or(REFLECTION_MEMORY_USER_ID)),
        created_after_ms: req.created_after_ms,
        created_before_ms: req.created_before_ms,
        labels_any: vec![],
        label_match: None,
        memory_kinds: vec![],
        limit: req.limit,
    };
    let mut request = build_thread_hybrid_request(&thread_req, vector);
    if let Some(filter) = request
        .options
        .as_mut()
        .and_then(|options| options.filter.as_mut())
    {
        filter.memory_kinds = vec![mem_data::MemoryKind::Reflection as i32];
    }
    request
}

fn reflection_memory_ids_from_hits(hits: Vec<mem_svc::MemorySearchResult>) -> Vec<i64> {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for hit in hits {
        let Some(memory_id) = hit.memory.and_then(|m| m.id).map(|id| id.value) else {
            continue;
        };
        if seen.insert(memory_id) {
            ids.push(memory_id);
        }
    }
    ids
}

/// Convert a proto `Reflection` to the trimmed UI DTO.
///
/// Returns `None` when:
///   - id / data / origin_thread_id is missing (defensive — server should
///     always populate these).
///   - summary is empty.
fn entry_from_proto(refl: mem_data::Reflection) -> Option<ReflectionEntry> {
    let id = refl.id?.value;
    let data = refl.data?;
    let origin_thread_id = data.origin_thread_id?.value;
    if data.summary.trim().is_empty() {
        return None;
    }
    Some(ReflectionEntry {
        id,
        origin_thread_id,
        summary: data.summary,
        task_intent: data.task_intent,
        task_category: data.task_category,
        reflection_aspect: data.reflection_aspect,
        outcome: data.outcome,
        score: data.score,
        score_self: data.score_self,
        score_heuristic: data.score_heuristic,
        lessons: data.lessons,
        key_decisions: data.key_decisions,
        success_factors: data.success_factors,
        failure_modes: data.failure_modes,
        mitigation_hint: data.mitigation_hint,
        pinned: data.pinned,
        prompt_version: data.prompt_version,
        intent_embedding_status: data.intent_embedding_status,
        created_at_ms: data.created_at,
        updated_at_ms: data.updated_at,
    })
}

fn selection_entry_from_proto(refl: mem_data::Reflection) -> Option<ReflectionSelectionEntry> {
    let id = refl.id?.value;
    let data = refl.data?;
    if data.origin_user_id.as_ref().map(|id| id.value) != Some(REFLECTION_MEMORY_USER_ID) {
        return None;
    }
    if data.summary.trim().is_empty() {
        return None;
    }
    let content = selection_content_from_data(id, &data);
    Some(ReflectionSelectionEntry {
        memory_id: id,
        origin_thread_id: data.origin_thread_id.as_ref().map(|id| id.value),
        summary: data.summary,
        content_json: content.content_json,
        source_thread_ids: content.source_thread_ids,
        source_memory_ids: content.source_memory_ids,
        created_at_ms: data.created_at,
        updated_at_ms: data.updated_at,
    })
}

fn reflection_selection_content_for_owner(
    reflection: mem_data::Reflection,
    owner_id: i64,
) -> Option<ReflectionSelectionContent> {
    let id = reflection.id?.value;
    let data = reflection.data?;
    (data.origin_user_id?.value == owner_id).then(|| selection_content_from_data(id, &data))
}

fn selection_content_from_data(
    memory_id: i64,
    data: &mem_data::ReflectionData,
) -> ReflectionSelectionContent {
    let origin_thread_id = data.origin_thread_id.as_ref().map(|id| id.value);
    let (source_thread_ids, source_memory_ids) = reflection_source_ids(data);
    ReflectionSelectionContent {
        memory_id,
        origin_thread_id,
        content_json: normalized_reflection_json(data),
        source_thread_ids,
        source_memory_ids,
    }
}

fn reflection_source_ids(data: &mem_data::ReflectionData) -> (Vec<String>, Vec<String>) {
    let source_thread_ids = data
        .origin_thread_id
        .as_ref()
        .map(|id| id.value.to_string())
        .into_iter()
        .collect();
    let mut source_memory_ids = Vec::new();
    let mut seen_memory_ids = HashSet::new();
    for fact in &data.facts {
        if let Some(anchor) = fact.anchor_memory_id.as_ref().map(|id| id.value)
            && seen_memory_ids.insert(anchor)
        {
            source_memory_ids.push(anchor.to_string());
        }
    }
    (source_thread_ids, source_memory_ids)
}

/// Produce a JSON object rather than forwarding protobuf/JSON wire details.
/// Int64 identifiers are strings so JavaScript cannot lose precision while
/// parsing a selected reflection snapshot.
fn normalized_reflection_json(data: &mem_data::ReflectionData) -> String {
    let origin_thread_id = data
        .origin_thread_id
        .as_ref()
        .map(|id| id.value.to_string());
    let facts = data
        .facts
        .iter()
        .map(|fact| {
            serde_json::json!({
                "turn_index": fact.turn_index,
                "anchor_memory_id": fact.anchor_memory_id.as_ref().map(|id| id.value.to_string()),
                "kind": fact.kind,
                "weight": fact.weight,
                "note": fact.note,
                "links": fact.links.iter().map(|link| serde_json::json!({
                    "field": link.field,
                    "index": link.index,
                })).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    let (source_thread_ids, source_memory_ids) = reflection_source_ids(data);
    serde_json::json!({
        "origin_thread_id": origin_thread_id,
        "source_thread_ids": source_thread_ids,
        "source_memory_ids": source_memory_ids,
        "origin_channel": data.origin_channel,
        "outcome": data.outcome,
        "score": data.score,
        "score_self": data.score_self,
        "score_heuristic": data.score_heuristic,
        "summary": data.summary,
        "task_intent": data.task_intent,
        "task_category": data.task_category,
        "reflection_aspect": data.reflection_aspect,
        "failure_modes": data.failure_modes,
        "failure_modes_other": data.failure_modes_other,
        "success_factors": data.success_factors,
        "lessons": data.lessons,
        "key_decisions": data.key_decisions,
        "tools_used": data.tools_used,
        "mitigation_hint": data.mitigation_hint,
        "is_recurrence": data.is_recurrence,
        "facts": facts,
        "tool_outcomes": data.tool_outcomes.iter().map(|entry| serde_json::json!({
            "tool": entry.tool,
            "contribution": entry.contribution,
            "error_kind": entry.error_kind,
        })).collect::<Vec<_>>(),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_reflection_request_deserializes_id_from_json_string() {
        let req: DeleteReflectionRequest =
            serde_json::from_str(r#"{"id":"9007199254740993"}"#).unwrap();
        assert_eq!(req.id, 9_007_199_254_740_993);
    }

    fn proto_with_summary(summary: &str) -> mem_data::Reflection {
        mem_data::Reflection {
            id: Some(mem_data::ReflectionId { value: 42 }),
            data: Some(mem_data::ReflectionData {
                origin_thread_id: Some(mem_data::ThreadId { value: 100 }),
                origin_user_id: Some(mem_data::UserId { value: 1 }),
                summary: summary.to_string(),
                task_intent: "intent".into(),
                task_category: 1,
                reflection_aspect: 1,
                outcome: 1,
                score: 0.8,
                score_self: 0.8,
                score_heuristic: 0.6,
                lessons: vec!["L1".into()],
                key_decisions: vec!["D1".into()],
                success_factors: vec!["F1".into()],
                failure_modes: vec![1, 2],
                mitigation_hint: Some("M".into()),
                pinned: true,
                prompt_version: "v1".into(),
                intent_embedding_status: 2,
                created_at: 1_700_000_000_000,
                updated_at: 1_700_000_000_001,
                ..Default::default()
            }),
        }
    }

    fn proto_with_facts() -> mem_data::Reflection {
        let mut reflection = proto_with_summary("selection summary");
        let data = reflection.data.as_mut().unwrap();
        data.facts = vec![
            mem_data::ReflectionFact {
                turn_index: 3,
                anchor_memory_id: Some(mem_data::MemoryId { value: 9001 }),
                ..Default::default()
            },
            mem_data::ReflectionFact {
                turn_index: 4,
                anchor_memory_id: Some(mem_data::MemoryId { value: 9001 }),
                ..Default::default()
            },
        ];
        reflection
    }

    fn memory_hit(memory_id: i64) -> mem_svc::MemorySearchResult {
        mem_svc::MemorySearchResult {
            memory: Some(mem_data::Memory {
                id: Some(mem_data::MemoryId { value: memory_id }),
                data: Some(mem_data::MemoryData {
                    content: "reflection summary".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn selection_request_accepts_omitted_cursor() {
        let request: ListReflectionsForSelectionRequest =
            serde_json::from_str(r#"{"limit":50}"#).expect("optional cursor should default");
        assert_eq!(request.limit, Some(50));
        assert_eq!(request.cursor_after_memory_id, None);
    }

    #[test]
    fn selection_list_request_is_filter_only_and_uses_memory_id_cursor() {
        let request = build_selection_list_request(&ListReflectionsForSelectionRequest {
            limit: Some(30),
            cursor_after_memory_id: Some(1234),
        });
        assert!(request.query_text.is_none());
        assert!(request.query_vectors.is_empty());
        assert_eq!(request.limit, Some(30));
        assert_eq!(request.cursor_after_memory_id, Some(1234));
        assert_eq!(
            request
                .filter
                .and_then(|filter| filter.origin_user_id)
                .map(|id| id.value),
            Some(REFLECTION_MEMORY_USER_ID)
        );
    }

    #[test]
    fn selection_page_filters_foreign_and_empty_rows_but_keeps_raw_cursor() {
        let mut foreign = proto_with_summary("foreign");
        foreign.id = Some(mem_data::ReflectionId { value: 20 });
        foreign.data.as_mut().unwrap().origin_user_id = Some(mem_data::UserId { value: 2 });
        let mut empty = proto_with_summary(" ");
        empty.id = Some(mem_data::ReflectionId { value: 19 });
        let mut own = proto_with_summary("own");
        own.id = Some(mem_data::ReflectionId { value: 18 });
        let page = selection_page_from_raw(
            vec![
                RawReflectionRow {
                    memory_id: Some(20),
                    reflection: Some(foreign),
                },
                RawReflectionRow {
                    memory_id: Some(19),
                    reflection: Some(empty),
                },
                RawReflectionRow {
                    memory_id: Some(18),
                    reflection: Some(own),
                },
            ],
            2,
        )
        .expect("page should remain addressable");
        assert_eq!(page.entries.len(), 0);
        // The second raw row is the page boundary even though both rows were
        // filtered, so the next request can reach the valid row.
        assert_eq!(page.next_cursor_after_memory_id, Some(19));
    }

    #[test]
    fn selection_page_uses_previous_id_when_boundary_row_has_no_id() {
        let mut own = proto_with_summary("own");
        own.id = Some(mem_data::ReflectionId { value: 20 });
        let mut malformed = proto_with_summary("malformed");
        malformed.id = None;
        let page = selection_page_from_raw(
            vec![
                RawReflectionRow {
                    memory_id: Some(20),
                    reflection: Some(own),
                },
                RawReflectionRow {
                    memory_id: None,
                    reflection: Some(malformed),
                },
                RawReflectionRow {
                    memory_id: Some(18),
                    reflection: None,
                },
            ],
            2,
        )
        .expect("the preceding row keeps the next page addressable");

        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.next_cursor_after_memory_id, Some(20));
    }

    #[test]
    fn selection_page_rejects_boundary_without_any_prior_id() {
        let mut first_malformed = proto_with_summary("first malformed");
        first_malformed.id = None;
        let mut boundary_malformed = proto_with_summary("boundary malformed");
        boundary_malformed.id = None;
        let mut later = proto_with_summary("later");
        later.id = Some(mem_data::ReflectionId { value: 18 });
        let error = selection_page_from_raw(
            vec![
                RawReflectionRow {
                    memory_id: None,
                    reflection: Some(first_malformed),
                },
                RawReflectionRow {
                    memory_id: None,
                    reflection: Some(boundary_malformed),
                },
                RawReflectionRow {
                    memory_id: Some(18),
                    reflection: Some(later),
                },
            ],
            2,
        )
        .expect_err("a page without a cursor cannot make progress");

        match error {
            AppError::Grpc(status) => {
                assert_eq!(status.code(), tonic::Code::DataLoss);
                assert!(status.message().contains("memory id"));
            }
            other => panic!("expected DataLoss, got {other:?}"),
        }
    }

    #[test]
    fn selection_page_returns_tail_without_cursor_when_raw_rows_fit() {
        let mut own = proto_with_summary("own");
        own.id = Some(mem_data::ReflectionId { value: 18 });
        let page = selection_page_from_raw(
            vec![RawReflectionRow {
                memory_id: Some(18),
                reflection: Some(own),
            }],
            2,
        )
        .expect("page should be returned");
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.next_cursor_after_memory_id, None);
    }

    #[test]
    fn selection_page_returns_last_raw_id_for_a_normal_full_page() {
        let rows = [18_i64, 17, 16]
            .into_iter()
            .map(|id| {
                let mut reflection = proto_with_summary("own");
                reflection.id = Some(mem_data::ReflectionId { value: id });
                RawReflectionRow {
                    memory_id: Some(id),
                    reflection: Some(reflection),
                }
            })
            .collect();
        let page = selection_page_from_raw(rows, 2).expect("page should be returned");
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.next_cursor_after_memory_id, Some(17));
    }

    #[test]
    fn selection_content_normalizes_ids_and_deduplicates_fact_anchors() {
        let reflection = proto_with_facts();
        let entry = selection_entry_from_proto(reflection).expect("selection entry");
        assert_eq!(entry.memory_id, 42);
        assert_eq!(entry.origin_thread_id, Some(100));
        assert_eq!(entry.source_thread_ids, vec!["100"]);
        assert_eq!(entry.source_memory_ids, vec!["9001"]);
        let json: serde_json::Value = serde_json::from_str(&entry.content_json).unwrap();
        assert_eq!(json["origin_thread_id"], "100");
        assert_eq!(json["facts"][0]["anchor_memory_id"], "9001");
        assert_eq!(json["facts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn selection_entry_drops_empty_summary_but_keeps_missing_origin_defensively() {
        let mut empty = proto_with_summary(" ");
        assert!(selection_entry_from_proto(empty).is_none());
        empty = proto_with_summary("usable");
        empty.data.as_mut().unwrap().origin_thread_id = None;
        let entry = selection_entry_from_proto(empty).expect("usable reflection");
        assert_eq!(entry.origin_thread_id, None);
        assert!(entry.source_thread_ids.is_empty());
    }

    #[test]
    fn selection_entry_rejects_foreign_origin_user() {
        let mut reflection = proto_with_summary("foreign");
        reflection.data.as_mut().unwrap().origin_user_id = Some(mem_data::UserId { value: 2 });
        assert!(selection_entry_from_proto(reflection).is_none());
    }

    #[test]
    fn selection_content_acl_rejects_reflections_owned_by_another_user() {
        let mut reflection = proto_with_facts();
        reflection.data.as_mut().unwrap().origin_user_id = Some(mem_data::UserId { value: 2 });
        assert!(
            reflection_selection_content_for_owner(reflection, REFLECTION_MEMORY_USER_ID).is_none()
        );

        let own = proto_with_facts();
        assert!(reflection_selection_content_for_owner(own, REFLECTION_MEMORY_USER_ID).is_some());
    }

    #[test]
    fn entry_from_proto_skips_empty_summary() {
        // Empty summaries are guarded out at this layer rather
        // than relying on the frontend to filter.
        assert!(entry_from_proto(proto_with_summary("")).is_none());
        assert!(entry_from_proto(proto_with_summary("   \n  ")).is_none());
    }

    #[test]
    fn entry_from_proto_skips_when_origin_thread_id_missing() {
        let mut r = proto_with_summary("ok");
        r.data.as_mut().unwrap().origin_thread_id = None;
        assert!(entry_from_proto(r).is_none());
    }

    #[test]
    fn entry_preserves_enum_values_for_frontend_mapping() {
        // Frontend `lib/searchTaxonomy.ts` maps these enum ints to
        // labels; preserving the raw int avoids string-translation drift
        // when proto enums grow.
        let entry = entry_from_proto(proto_with_summary("summary text")).expect("non-empty");
        assert_eq!(entry.task_category, 1);
        assert_eq!(entry.outcome, 1);
        assert_eq!(entry.reflection_aspect, 1);
        assert_eq!(entry.failure_modes, vec![1, 2]);
        assert_eq!(entry.intent_embedding_status, 2);
    }

    #[test]
    fn build_search_request_uses_origin_user_id_1_by_default() {
        let req = SearchReflectionsRequest::default();
        let proto = build_search_request(&req);
        let filter = proto.filter.expect("filter always set");
        assert_eq!(filter.origin_user_id.map(|u| u.value), Some(1));
    }

    #[test]
    fn build_search_request_maps_outcome_and_dates() {
        let req = SearchReflectionsRequest {
            query_text: None,
            user_id: Some(7),
            outcomes: vec![3, 4],
            created_after_ms: Some(1_700_000_000_000),
            created_before_ms: Some(1_800_000_000_000),
            limit: Some(50),
        };
        let proto = build_search_request(&req);
        assert_eq!(proto.limit, Some(50));
        let filter = proto.filter.unwrap();
        assert_eq!(filter.origin_user_id.map(|u| u.value), Some(7));
        assert_eq!(filter.outcomes, vec![3, 4]);
        assert_eq!(filter.created_after, Some(1_700_000_000_000));
        assert_eq!(filter.created_before, Some(1_800_000_000_000));
    }

    #[test]
    fn build_search_request_empty_query_text_becomes_none() {
        // reflection.proto:121-126 — supplying neither text nor vectors
        // turns the call into filter-only listing. Forwarding an empty
        // string instead would make the server try to BM25-rank "".
        for q in ["", "   ", "\t\n"] {
            let req = SearchReflectionsRequest {
                query_text: Some(q.into()),
                ..Default::default()
            };
            assert_eq!(build_search_request(&req).query_text, None);
        }
    }

    #[test]
    fn build_search_request_passes_through_real_query() {
        let req = SearchReflectionsRequest {
            query_text: Some("  rust ownership  ".into()),
            ..Default::default()
        };
        // Trimmed so the server sees a clean term.
        assert_eq!(
            build_search_request(&req).query_text,
            Some("rust ownership".into())
        );
    }

    #[test]
    fn build_hybrid_search_request_targets_reflection_memory_owner() {
        let req = SearchReflectionsHybridRequest {
            query_text: "flaky test recovery".into(),
            created_after_ms: Some(1_000),
            created_before_ms: Some(2_000),
            limit: Some(25),
            ..Default::default()
        };
        let r = build_hybrid_search_request(&req, vec![0.1, 0.2, 0.3]);
        assert_eq!(r.query_text, "flaky test recovery");
        assert_eq!(r.query_vectors.len(), 1);
        assert_eq!(r.query_vectors[0].values, vec![0.1, 0.2, 0.3]);
        assert!(r.hybrid_options.is_none());
        assert!(r.fts_options.is_none());
        let opts = r.options.expect("options always set");
        assert_eq!(opts.limit, 25);
        assert_eq!(opts.include_content, Some(true));
        let filter = opts.filter.expect("filter always set");
        assert_eq!(filter.user_id, Some(REFLECTION_MEMORY_USER_ID));
        assert_eq!(
            filter.memory_kinds,
            vec![mem_data::MemoryKind::Reflection as i32]
        );
        assert_eq!(filter.created_after, Some(1_000));
        assert_eq!(filter.created_before, Some(2_000));
        assert!(filter.thread_filter.is_none());
    }

    #[test]
    fn build_hybrid_search_request_overrides_reflection_memory_owner() {
        let req = SearchReflectionsHybridRequest {
            query_text: "owned search".into(),
            user_id: Some(300_123),
            ..Default::default()
        };
        let r = build_hybrid_search_request(&req, vec![0.1]);
        let filter = r.options.unwrap().filter.unwrap();
        assert_eq!(filter.user_id, Some(300_123));
    }

    #[test]
    fn reflection_memory_ids_preserve_order_and_dedupe() {
        let ids = reflection_memory_ids_from_hits(vec![
            memory_hit(10),
            memory_hit(20),
            memory_hit(10),
            mem_svc::MemorySearchResult::default(),
            memory_hit(30),
        ]);
        assert_eq!(ids, vec![10, 20, 30]);
    }

    #[test]
    fn build_intent_request_rejects_empty_intent() {
        for q in ["", "   ", "\t\n"] {
            let req = SearchReflectionsByIntentRequest {
                intent_text: q.into(),
                ..Default::default()
            };
            let err = build_intent_request(&req).unwrap_err();
            assert!(matches!(err, AppError::Config(_)));
        }
    }

    #[test]
    fn build_intent_request_trims_and_defaults_top_k() {
        let req = SearchReflectionsByIntentRequest {
            intent_text: "  how to fix flaky tests  ".into(),
            ..Default::default()
        };
        let r = build_intent_request(&req).unwrap();
        assert_eq!(r.intent_text, "how to fix flaky tests");
        assert_eq!(r.top_k, 20);
        let filter = r.filter.expect("filter always set");
        assert_eq!(filter.origin_user_id.map(|u| u.value), Some(1));
    }

    #[test]
    fn build_intent_request_forwards_filter_and_top_k() {
        let req = SearchReflectionsByIntentRequest {
            intent_text: "x".into(),
            top_k: Some(5),
            user_id: Some(9),
            outcomes: vec![2, 3],
            created_after_ms: Some(1_000),
            created_before_ms: Some(2_000),
        };
        let r = build_intent_request(&req).unwrap();
        assert_eq!(r.top_k, 5);
        let filter = r.filter.unwrap();
        assert_eq!(filter.origin_user_id.map(|u| u.value), Some(9));
        assert_eq!(filter.outcomes, vec![2, 3]);
        assert_eq!(filter.created_after, Some(1_000));
        assert_eq!(filter.created_before, Some(2_000));
    }

    #[test]
    fn build_redispatch_request_defaults_to_intent_kind() {
        // No kind supplied → INTENT(2): the natural-language search reads the
        // intent table, so that's the useful default for a "make search work"
        // backfill.
        let req = RedispatchReflectionEmbeddingsRequest::default();
        let r = build_redispatch_request(&req).unwrap();
        assert_eq!(r.kind, 2);
    }

    #[test]
    fn build_redispatch_request_rejects_unspecified_kind() {
        // UNSPECIFIED(0) is rejected before the round-trip (the server rejects
        // it too, but a local guard gives a clearer error).
        let req = RedispatchReflectionEmbeddingsRequest {
            kind: Some(0),
            ..Default::default()
        };
        let err = build_redispatch_request(&req).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    #[test]
    fn build_redispatch_request_accepts_both_kind() {
        let req = RedispatchReflectionEmbeddingsRequest {
            kind: Some(3),
            ..Default::default()
        };
        assert_eq!(build_redispatch_request(&req).unwrap().kind, 3);
    }

    #[test]
    fn build_redispatch_request_reuses_reflection_filter() {
        // Scope (user/outcomes/period) flows through the shared filter builder,
        // and batch_size passes through, so the backfill matches what the
        // Reflections tab filters on.
        let req = RedispatchReflectionEmbeddingsRequest {
            kind: Some(2),
            user_id: Some(9),
            outcomes: vec![1, 4],
            created_after_ms: Some(1_000),
            created_before_ms: Some(2_000),
            batch_size: Some(64),
        };
        let r = build_redispatch_request(&req).unwrap();
        assert_eq!(r.batch_size, Some(64));
        let filter = r.filter.expect("filter always set");
        assert_eq!(filter.origin_user_id.map(|u| u.value), Some(9));
        assert_eq!(filter.outcomes, vec![1, 4]);
        assert_eq!(filter.created_after, Some(1_000));
        assert_eq!(filter.created_before, Some(2_000));
    }

    #[test]
    fn build_redispatch_request_defaults_user_id_to_1() {
        // Mirrors build_reflection_filter's default so an unscoped backfill
        // still targets the single local user (origin_user_id=1).
        let req = RedispatchReflectionEmbeddingsRequest::default();
        let filter = build_redispatch_request(&req).unwrap().filter.unwrap();
        assert_eq!(filter.origin_user_id.map(|u| u.value), Some(1));
    }

    #[test]
    fn stats_from_proto_projects_embedding_counts() {
        let proto = mem_svc::IndexStatsResponse {
            total_records: 100,
            records_with_embedding: 80,
            records_without_embedding: 20,
            vector_dimension: 2048,
            ..Default::default()
        };
        let stats = stats_from_proto(proto);
        assert_eq!(
            stats,
            ReflectionIntentIndexStats {
                total_records: 100,
                records_with_embedding: 80,
                records_without_embedding: 20,
                vector_dimension: 2048,
            }
        );
    }
}
