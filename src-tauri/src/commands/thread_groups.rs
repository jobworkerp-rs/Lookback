//! Tauri read commands for the primary ThreadGroup UI.

#[cfg(test)]
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use tauri::State;
use tokio_stream::StreamExt;

use crate::error::{AppError, AppResult};
use crate::grpc::proto::llm_memory::data as mem_data;
use crate::grpc::proto::llm_memory::service as mem_svc;
use crate::grpc::proto::llm_memory::service::thread_group_service_client::ThreadGroupServiceClient;
use tonic::transport::Channel;

use super::AppState;
use super::memories_rpc_support::{MemoriesRpc, is_rpc_unsupported};
use super::search::SearchMode;
use super::thread_group_owner::{
    resolve_group_owner_user_id, validate_optional_user_id, validate_positive_user_id,
};
use super::threads::LabelMatch;

async fn owner_scoped_thread_group_client(
    channel: Channel,
) -> AppResult<ThreadGroupServiceClient<Channel>> {
    super::connection::require_thread_group_owner_capability(channel.clone()).await?;
    Ok(ThreadGroupServiceClient::new(channel))
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListThreadGroupsRequest {
    pub limit: Option<i32>,
    pub offset: Option<i64>,
    pub group_owner_user_id: Option<i64>,
    /// Legacy alias for `group_owner_user_id`.
    pub user_id: Option<i64>,
    #[serde(default)]
    pub include_inactive: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchThreadGroupsRequest {
    pub query_text: String,
    pub mode: SearchMode,
    /// Owner used to select groups. `user_id` is retained as a legacy alias.
    pub group_owner_user_id: Option<i64>,
    /// Optional owner restriction for member Thread/Memory matching.
    /// Unset searches every owner represented in the selected groups.
    pub member_user_id: Option<i64>,
    pub user_id: Option<i64>,
    pub created_after_ms: Option<i64>,
    pub created_before_ms: Option<i64>,
    #[serde(default)]
    pub labels_any: Vec<String>,
    #[serde(default)]
    pub label_match: Option<LabelMatch>,
    /// The backend defaults to same-member matching; cross-member matching is
    /// an explicit opt-in because it can join unrelated member evidence.
    #[serde(default)]
    pub allow_cross_member: bool,
    pub page_size: Option<i32>,
    pub page_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadGroupThreadDisplay {
    #[serde(with = "crate::serde_id::option")]
    pub thread_id: Option<i64>,
    pub thread_canonical_key: String,
    pub description: Option<String>,
    pub source: Option<String>,
    pub created_at_ms: Option<i64>,
    pub last_message_at_ms: Option<i64>,
    pub deleted_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadGroupListItem {
    #[serde(with = "crate::serde_id")]
    pub id: i64,
    pub user_id: i64,
    pub group_canonical_key: String,
    pub title: Option<String>,
    pub display_name: String,
    pub status: String,
    pub grouping_authority: String,
    #[serde(with = "crate::serde_id::option")]
    pub redirect_to_group_id: Option<i64>,
    pub latest_activity_at_ms: Option<i64>,
    #[serde(with = "crate::serde_id::option")]
    pub root_thread_id: Option<i64>,
    pub root_thread_canonical_key: Option<String>,
    pub root_display: Option<ThreadGroupThreadDisplay>,
    pub active_member_count: i64,
    pub deleted_member_count: i64,
    pub unresolved_count: i64,
    pub membership_snapshot_digest: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadGroupMemberView {
    #[serde(with = "crate::serde_id")]
    pub group_id: i64,
    pub user_id: i64,
    #[serde(with = "crate::serde_id::option")]
    pub thread_id: Option<i64>,
    pub thread_canonical_key: String,
    pub source: Option<String>,
    pub role: String,
    pub state: String,
    pub provenance: String,
    pub identity_scope: Option<String>,
    pub native_id: Option<String>,
    #[serde(with = "crate::serde_id::option")]
    pub deleted_at_ms: Option<i64>,
    pub display: Option<ThreadGroupThreadDisplay>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadRelationView {
    #[serde(with = "crate::serde_id")]
    pub id: i64,
    #[serde(with = "crate::serde_id::option")]
    pub parent_thread_id: Option<i64>,
    #[serde(with = "crate::serde_id::option")]
    pub child_thread_id: Option<i64>,
    pub parent_thread_canonical_key: String,
    pub child_thread_canonical_key: String,
    pub parent_user_id: i64,
    pub child_user_id: i64,
    pub relation_type: String,
    pub state: String,
    pub selection_basis: String,
    pub source_confidence: Option<String>,
    #[serde(with = "crate::serde_id::option")]
    pub selected_observation_id: Option<i64>,
    #[serde(with = "crate::serde_id::option")]
    pub selected_operator_decision_id: Option<i64>,
    #[serde(with = "crate::serde_id::option")]
    pub parent_group_id: Option<i64>,
    #[serde(with = "crate::serde_id::option")]
    pub child_group_id: Option<i64>,
    pub parent_display: Option<ThreadGroupThreadDisplay>,
    pub child_display: Option<ThreadGroupThreadDisplay>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadGroupUnresolvedView {
    #[serde(with = "crate::serde_id")]
    pub id: i64,
    #[serde(with = "crate::serde_id::option")]
    pub subject_thread_id: Option<i64>,
    pub subject_source: String,
    pub subject_user_id: i64,
    #[serde(with = "crate::serde_id::option")]
    pub candidate_group_id: Option<i64>,
    #[serde(with = "crate::serde_id::option")]
    pub candidate_parent_thread_id: Option<i64>,
    pub state: String,
    pub subject_identity_scope_known: bool,
    pub subject_identity_scope_value: String,
    pub subject_native_id: String,
    #[serde(with = "crate::serde_id::option")]
    pub selected_observation_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadObservationView {
    #[serde(with = "crate::serde_id")]
    pub id: i64,
    pub subject_source: String,
    pub subject_user_id: i64,
    pub subject_identity_scope_known: bool,
    pub subject_identity_scope_value: String,
    pub subject_native_id: String,
    pub candidate_parent_present: bool,
    pub candidate_parent_user_id: Option<i64>,
    pub candidate_parent_source: Option<String>,
    pub candidate_parent_identity_scope_known: bool,
    pub candidate_parent_identity_scope_value: Option<String>,
    pub candidate_parent_native_id: Option<String>,
    pub relation_kind: Option<String>,
    pub evidence_kind: String,
    pub polarity: String,
    pub source_confidence: Option<String>,
    pub state: String,
    pub adapter_version: String,
    pub source_record_ref: String,
    pub observed_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadGroupLineageView {
    pub group: ThreadGroupListItem,
    pub members: Vec<ThreadGroupMemberView>,
    pub relations: Vec<ThreadRelationView>,
    pub unresolved: Vec<ThreadGroupUnresolvedView>,
    pub observations: Vec<ThreadObservationView>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadGroupSummaryView {
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
    pub external_id: Option<String>,
    pub content_json: String,
    pub metadata: Option<String>,
    pub updated_at_ms: i64,
    pub membership_snapshot_digest: Option<String>,
    #[serde(with = "crate::serde_id::option")]
    pub group_id: Option<i64>,
    pub active_member_count: Option<i64>,
    pub deleted_member_count: Option<i64>,
    pub unresolved_count: Option<i64>,
    pub member_summary_count: Option<i64>,
    pub summary_version: Option<String>,
    pub aggregation_mode: Option<String>,
    #[serde(with = "crate::serde_id::option")]
    pub passthrough_source_memory_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadGroupSearchWitnessView {
    pub predicate_index: i32,
    pub target: String,
    #[serde(with = "crate::serde_id")]
    pub thread_id: i64,
    pub thread_canonical_key: String,
    pub score: f32,
    pub member: Option<ThreadGroupThreadDisplay>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadGroupSearchResultView {
    pub group: ThreadGroupListItem,
    pub relevance_score: f32,
    pub witnesses: Vec<ThreadGroupSearchWitnessView>,
    pub root_member: Option<ThreadGroupThreadDisplay>,
    pub current_member_count: i64,
    pub active_member_count: i64,
    pub deleted_member_count: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadGroupSearchPage {
    pub results: Vec<ThreadGroupSearchResultView>,
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FindThreadGroupRequest {
    #[serde(with = "crate::serde_id")]
    pub group_id: i64,
    pub group_owner_user_id: Option<i64>,
    /// Legacy alias for `group_owner_user_id`.
    pub user_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FindThreadGroupSummaryRequest {
    #[serde(with = "crate::serde_id")]
    pub group_id: i64,
    pub group_owner_user_id: Option<i64>,
    /// Legacy alias for `group_owner_user_id`.
    pub user_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FindThreadGroupSummariesRequest {
    #[serde(with = "crate::serde_id::vec")]
    pub group_ids: Vec<i64>,
    pub group_owner_user_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadGroupSummaryEntryView {
    #[serde(with = "crate::serde_id")]
    pub group_id: i64,
    pub summary: Option<ThreadGroupSummaryView>,
}

#[tauri::command]
pub async fn list_thread_groups(
    state: State<'_, AppState>,
    req: ListThreadGroupsRequest,
) -> AppResult<Vec<ThreadGroupListItem>> {
    let group_owner_user_id = resolve_group_owner_user_id(req.group_owner_user_id, req.user_id)?;
    let mut client = owner_scoped_thread_group_client(state.memories_channel().await?).await?;
    let mut stream = client
        .find_thread_group_list(mem_svc::FindThreadGroupListRequest {
            limit: req.limit,
            offset: req.offset,
            include_inactive: req.include_inactive,
            user_id: Some(group_owner_user_id),
        })
        .await
        .map_err(map_thread_group_status)?
        .into_inner();
    let mut groups = Vec::new();
    while let Some(group) = stream.next().await {
        groups.push(map_group(group?)?);
    }
    Ok(groups)
}

#[tauri::command]
pub async fn search_thread_groups(
    state: State<'_, AppState>,
    req: SearchThreadGroupsRequest,
) -> AppResult<ThreadGroupSearchPage> {
    let group_owner_user_id = resolve_group_owner_user_id(req.group_owner_user_id, req.user_id)?;
    validate_optional_user_id("member_user_id", req.member_user_id)?;
    match req.mode {
        SearchMode::Keyword => {
            search_thread_groups_with_vector(&state, &req, group_owner_user_id, Vec::new()).await
        }
        SearchMode::Semantic | SearchMode::Hybrid => {
            state.ensure_local_embedding_available()?;
            super::embedding_space::with_query_embedding(&state, &req.query_text, |query_vector| {
                search_thread_groups_with_vector(&state, &req, group_owner_user_id, query_vector)
            })
            .await
        }
    }
}

async fn search_thread_groups_with_vector(
    state: &AppState,
    req: &SearchThreadGroupsRequest,
    group_owner_user_id: i64,
    query_vector: Vec<f32>,
) -> AppResult<ThreadGroupSearchPage> {
    let mut client = owner_scoped_thread_group_client(state.memories_channel().await?).await?;
    let response = client
        .search_thread_groups(build_group_search_request(
            req,
            query_vector,
            group_owner_user_id,
        ))
        .await
        .map_err(map_thread_group_status)?
        .into_inner();
    let results = response
        .results
        .into_iter()
        .map(map_search_result)
        .collect::<AppResult<Vec<_>>>()?;
    Ok(ThreadGroupSearchPage {
        results,
        next_page_token: response.next_page_token,
    })
}

#[tauri::command]
pub async fn find_thread_group(
    state: State<'_, AppState>,
    req: FindThreadGroupRequest,
) -> AppResult<ThreadGroupLineageView> {
    validate_positive_user_id("group_id", req.group_id)?;
    let group_owner_user_id = resolve_group_owner_user_id(req.group_owner_user_id, req.user_id)?;
    let mut client = owner_scoped_thread_group_client(state.memories_channel().await?).await?;
    let lineage = client
        .find_thread_group_lineage(mem_data::ThreadGroupId {
            value: req.group_id,
            user_id: Some(group_owner_user_id),
        })
        .await
        .map_err(map_thread_group_status)?
        .into_inner();
    map_lineage(lineage)
}

#[tauri::command]
pub async fn find_thread_group_summary(
    state: State<'_, AppState>,
    req: FindThreadGroupSummaryRequest,
) -> AppResult<Option<ThreadGroupSummaryView>> {
    validate_positive_user_id("group_id", req.group_id)?;
    let group_owner_user_id = resolve_group_owner_user_id(req.group_owner_user_id, req.user_id)?;
    let summary = find_summaries(&state, &[req.group_id], group_owner_user_id)
        .await?
        .pop()
        .flatten();
    summary.map(map_summary).transpose()
}

/// Summaries for every group card on a page in one round trip; entries are
/// returned in request order.
#[tauri::command]
pub async fn find_thread_group_summaries(
    state: State<'_, AppState>,
    req: FindThreadGroupSummariesRequest,
) -> AppResult<Vec<ThreadGroupSummaryEntryView>> {
    for group_id in &req.group_ids {
        validate_positive_user_id("group_id", *group_id)?;
    }
    let group_owner_user_id = resolve_group_owner_user_id(req.group_owner_user_id, None)?;
    let summaries = find_summaries(&state, &req.group_ids, group_owner_user_id).await?;
    req.group_ids
        .into_iter()
        .zip(summaries)
        .map(|(group_id, summary)| {
            Ok(ThreadGroupSummaryEntryView {
                group_id,
                summary: summary.map(map_summary).transpose()?,
            })
        })
        .collect()
}

/// The server rejects larger batches with INVALID_ARGUMENT.
const THREAD_GROUP_SUMMARY_BATCH_LIMIT: usize = 100;

async fn find_summaries(
    state: &AppState,
    group_ids: &[i64],
    group_owner_user_id: i64,
) -> AppResult<Vec<Option<mem_data::Memory>>> {
    if group_ids.is_empty() {
        return Ok(Vec::new());
    }
    let (memories_url, channel) = state.memories_endpoint().await?;
    let client = owner_scoped_thread_group_client(channel).await?;
    let entries = state
        .unsupported_memories_rpcs()
        .call(
            &memories_url,
            MemoriesRpc::FindThreadGroupSummaries,
            || batch_summary_entries(client.clone(), group_ids, group_owner_user_id),
            map_thread_group_status,
            // Fallback for memories servers predating FindThreadGroupSummaries;
            // remove once the minimum supported memories includes it.
            || legacy_summary_entries(client.clone(), group_ids, group_owner_user_id),
        )
        .await?;
    align_summary_entries(group_ids, entries)
}

async fn batch_summary_entries(
    mut client: ThreadGroupServiceClient<Channel>,
    group_ids: &[i64],
    group_owner_user_id: i64,
) -> Result<Vec<mem_svc::ThreadGroupSummaryEntry>, tonic::Status> {
    let mut entries = Vec::with_capacity(group_ids.len());
    for batch in group_ids.chunks(THREAD_GROUP_SUMMARY_BATCH_LIMIT) {
        let response = client
            .find_thread_group_summaries(mem_svc::FindThreadGroupSummariesRequest {
                group_ids: batch.to_vec(),
                user_id: Some(group_owner_user_id),
            })
            .await?;
        entries.extend(response.into_inner().entries);
    }
    Ok(entries)
}

/// Bounds the per-group fallback so a large page cannot open one request per
/// group at once against an older server.
const LEGACY_SUMMARY_CONCURRENCY: usize = 8;

async fn legacy_summary_entries(
    client: ThreadGroupServiceClient<Channel>,
    group_ids: &[i64],
    group_owner_user_id: i64,
) -> AppResult<Vec<mem_svc::ThreadGroupSummaryEntry>> {
    use futures::{StreamExt as _, TryStreamExt as _};
    // Collected up front: a stream holding the mapping closure fails the
    // higher-ranked `Send` bound required of tauri command futures.
    let reads: Vec<_> = group_ids
        .iter()
        .map(|&group_id| {
            let mut client = client.clone();
            async move {
                // Deprecated single-group read, kept only for memories servers
                // predating FindThreadGroupSummaries; remove once the minimum
                // supported memories includes it.
                #[allow(deprecated)]
                let response = client
                    .find_thread_group_summary(mem_svc::FindThreadGroupSummaryRequest {
                        group_id,
                        user_id: Some(group_owner_user_id),
                    })
                    .await
                    .map_err(map_thread_group_status)?;
                Ok(mem_svc::ThreadGroupSummaryEntry {
                    group_id,
                    summary: response.into_inner().summary,
                })
            }
        })
        .collect();
    // `buffered` keeps request order, which align_summary_entries relies on.
    futures::stream::iter(reads)
        .buffered(LEGACY_SUMMARY_CONCURRENCY)
        .try_collect()
        .await
}

/// Cards are matched to summaries by position, so a response that does not
/// mirror the request exactly would attach summaries to the wrong groups.
fn align_summary_entries(
    group_ids: &[i64],
    entries: Vec<mem_svc::ThreadGroupSummaryEntry>,
) -> AppResult<Vec<Option<mem_data::Memory>>> {
    if entries.len() != group_ids.len()
        || entries
            .iter()
            .zip(group_ids)
            .any(|(entry, group_id)| entry.group_id != *group_id)
    {
        return Err(AppError::Grpc(tonic::Status::internal(
            "ThreadGroup summary batch response does not match the requested groups",
        )));
    }
    Ok(entries.into_iter().map(|entry| entry.summary).collect())
}

/// Count only summaries attached to active groups with live members. This is
/// intentionally separate from the paged summary list used by the page.
#[tauri::command]
pub async fn count_thread_group_summaries(
    state: State<'_, AppState>,
    group_owner_user_id: Option<i64>,
    user_id: Option<i64>,
) -> AppResult<i64> {
    let group_owner_user_id = resolve_count_group_owner_user_id(group_owner_user_id, user_id)?;
    let mut client = owner_scoped_thread_group_client(state.memories_channel().await?).await?;
    let response = client
        .count_thread_group_summaries(mem_svc::CountThreadGroupSummariesRequest {
            user_id: Some(group_owner_user_id),
        })
        .await
        .map_err(map_thread_group_status)?
        .into_inner();
    Ok(response.count)
}

/// The Threads badge is the active-group count, not the number of paged rows.
#[tauri::command]
pub async fn count_active_thread_groups(
    state: State<'_, AppState>,
    group_owner_user_id: Option<i64>,
    user_id: Option<i64>,
) -> AppResult<i64> {
    let group_owner_user_id = resolve_count_group_owner_user_id(group_owner_user_id, user_id)?;
    let (memories_url, channel) = state.memories_endpoint().await?;
    let client = owner_scoped_thread_group_client(channel).await?;
    state
        .unsupported_memories_rpcs()
        .call(
            &memories_url,
            MemoriesRpc::CountThreadGroups,
            || {
                let mut client = client.clone();
                async move {
                    let response = client
                        .count_thread_groups(mem_svc::CountThreadGroupsRequest {
                            user_id: Some(group_owner_user_id),
                        })
                        .await?;
                    Ok(response.into_inner().count)
                }
            },
            map_thread_group_status,
            // Fallback for memories servers predating CountThreadGroups;
            // remove once the minimum supported memories includes it.
            || stream_active_group_count(client.clone(), group_owner_user_id),
        )
        .await
}

async fn stream_active_group_count(
    mut client: ThreadGroupServiceClient<Channel>,
    group_owner_user_id: i64,
) -> AppResult<i64> {
    let mut stream = client
        .find_thread_group_list(active_group_list_request(group_owner_user_id))
        .await
        .map_err(map_thread_group_status)?
        .into_inner();
    let mut count = 0_i64;
    while stream
        .message()
        .await
        .map_err(map_thread_group_status)?
        .is_some()
    {
        count += 1;
    }
    Ok(count)
}

fn resolve_count_group_owner_user_id(
    group_owner_user_id: Option<i64>,
    user_id: Option<i64>,
) -> AppResult<i64> {
    resolve_group_owner_user_id(group_owner_user_id, user_id)
}

fn active_group_list_request(user_id: i64) -> mem_svc::FindThreadGroupListRequest {
    mem_svc::FindThreadGroupListRequest {
        user_id: Some(user_id),
        ..Default::default()
    }
}

fn build_group_search_request(
    req: &SearchThreadGroupsRequest,
    query_vector: Vec<f32>,
    group_owner_user_id: i64,
) -> mem_svc::SearchThreadGroupsRequest {
    let member_user_id = req.member_user_id;
    let memory_kinds = vec![mem_data::MemoryKind::Raw as i32];
    let browse_filter = req
        .query_text
        .trim()
        .is_empty()
        .then(|| mem_data::ThreadSearchFilter {
            user_id: member_user_id,
            labels: req.labels_any.clone(),
            label_match_mode: Some(req.label_match.unwrap_or(LabelMatch::Any).to_proto()),
            channel: None,
            thread_created_after: req.created_after_ms,
            thread_created_before: req.created_before_ms,
            thread_updated_after: None,
            thread_updated_before: None,
            first_message_after: None,
            first_message_before: None,
            last_message_after: None,
            last_message_before: None,
            memory_kinds: memory_kinds.clone(),
            thread_group_id: None,
            thread_id: None,
        });
    let thread_filter = (!req.labels_any.is_empty() || member_user_id.is_some()).then(|| {
        mem_data::ThreadSearchFilter {
            user_id: member_user_id,
            labels: req.labels_any.clone(),
            label_match_mode: Some(req.label_match.unwrap_or(LabelMatch::Any).to_proto()),
            channel: None,
            thread_created_after: None,
            thread_created_before: None,
            thread_updated_after: None,
            thread_updated_before: None,
            first_message_after: None,
            first_message_before: None,
            last_message_after: None,
            last_message_before: None,
            memory_kinds: memory_kinds.clone(),
            thread_group_id: None,
            thread_id: None,
        }
    });
    let memory_filter = mem_data::MemorySearchFilter {
        user_id: member_user_id,
        roles: Vec::new(),
        content_types: Vec::new(),
        created_after: req.created_after_ms,
        created_before: req.created_before_ms,
        updated_after: None,
        updated_before: None,
        thread_filter,
        memory_kinds,
        external_id_prefix: None,
    };
    let mode = match req.mode {
        SearchMode::Keyword => mem_svc::ThreadGroupSearchMode::Keyword,
        SearchMode::Semantic => mem_svc::ThreadGroupSearchMode::Semantic,
        SearchMode::Hybrid => mem_svc::ThreadGroupSearchMode::Hybrid,
    };
    mem_svc::SearchThreadGroupsRequest {
        predicates: if browse_filter.is_some() {
            Vec::new()
        } else {
            vec![mem_svc::ThreadGroupMemberSearchQuery {
                target: mem_svc::ThreadGroupSearchTarget::Memory as i32,
                mode: mode as i32,
                query_text: req.query_text.clone(),
                query_vectors: if query_vector.is_empty() {
                    Vec::new()
                } else {
                    vec![mem_data::EmbeddingVector {
                        values: query_vector,
                    }]
                },
                thread_filter: None,
                memory_filter: Some(memory_filter),
                hybrid_options: None,
            }]
        },
        allow_cross_member: req.allow_cross_member,
        page_size: req.page_size,
        page_token: req.page_token.clone(),
        browse_filter,
        user_id: Some(group_owner_user_id),
    }
}

fn map_search_result(
    result: mem_svc::ThreadGroupSearchResult,
) -> AppResult<ThreadGroupSearchResultView> {
    let group = result.group.ok_or_else(|| {
        AppError::Config("ThreadGroup search result did not contain a group".into())
    })?;
    Ok(ThreadGroupSearchResultView {
        group: map_group(group)?,
        relevance_score: result.relevance_score,
        witnesses: result
            .witnesses
            .into_iter()
            .map(map_search_witness)
            .collect::<AppResult<Vec<_>>>()?,
        root_member: result.root_member.map(map_display).transpose()?,
        current_member_count: result.current_member_count,
        active_member_count: result.active_member_count,
        deleted_member_count: result.deleted_member_count,
    })
}

fn map_search_witness(
    witness: mem_svc::ThreadGroupSearchWitness,
) -> AppResult<ThreadGroupSearchWitnessView> {
    Ok(ThreadGroupSearchWitnessView {
        predicate_index: witness.predicate_index,
        target: enum_name(witness.target, &["unspecified", "thread", "memory"]),
        thread_id: positive_response_id("search witness thread_id", witness.thread_id)?,
        thread_canonical_key: witness.thread_canonical_key,
        score: witness.score,
        member: witness.member.map(map_display).transpose()?,
    })
}

fn map_lineage(lineage: mem_data::ThreadGroupLineage) -> AppResult<ThreadGroupLineageView> {
    let group = lineage
        .group
        .ok_or_else(|| AppError::Config("ThreadGroup lineage did not contain a group".into()))?;
    Ok(ThreadGroupLineageView {
        group: map_group(group)?,
        members: lineage
            .members
            .into_iter()
            .map(map_member)
            .collect::<AppResult<Vec<_>>>()?,
        relations: lineage
            .relations
            .into_iter()
            .map(map_relation)
            .collect::<AppResult<Vec<_>>>()?,
        unresolved: lineage
            .unresolved
            .into_iter()
            .map(map_unresolved)
            .collect::<AppResult<Vec<_>>>()?,
        observations: lineage
            .observations
            .into_iter()
            .map(map_observation)
            .collect::<AppResult<Vec<_>>>()?,
    })
}

fn map_group(group: mem_data::ThreadGroup) -> AppResult<ThreadGroupListItem> {
    let display_name = group_display_name(&group);
    let group_id = group.id.ok_or_else(|| {
        AppError::Config("ThreadGroup response did not contain a group id".into())
    })?;
    let id = positive_response_id("ThreadGroup id", group_id.value)?;
    let id_owner = required_positive_response_id("ThreadGroup id user_id", group_id.user_id)?;
    let user_id = required_positive_response_id("ThreadGroup user_id", group.user_id)?;
    if id_owner != user_id {
        return Err(AppError::Config(
            "ThreadGroup response group owner fields do not match".into(),
        ));
    }

    Ok(ThreadGroupListItem {
        id,
        user_id,
        group_canonical_key: group.group_canonical_key,
        title: group.title,
        display_name,
        status: enum_name(
            group.status,
            &["unspecified", "active", "redirected", "split"],
        ),
        grouping_authority: enum_name(
            group.grouping_authority,
            &["unspecified", "reconciler", "operator"],
        ),
        redirect_to_group_id: group.redirect_to_group_id,
        latest_activity_at_ms: group.latest_activity_at,
        root_thread_id: group.root_thread_id,
        root_thread_canonical_key: group.root_thread_canonical_key,
        root_display: group.root_display.map(map_display).transpose()?,
        active_member_count: group.active_member_count,
        deleted_member_count: group.deleted_member_count,
        unresolved_count: group.unresolved_count,
        membership_snapshot_digest: group.membership_snapshot_digest,
    })
}

fn map_display(display: mem_data::ThreadGroupThreadDisplay) -> AppResult<ThreadGroupThreadDisplay> {
    Ok(ThreadGroupThreadDisplay {
        thread_id: optional_positive_response_id(
            "ThreadGroup display thread_id",
            display.thread_id,
        )?,
        thread_canonical_key: display.thread_canonical_key,
        description: display.description,
        source: display.source,
        created_at_ms: display.created_at,
        last_message_at_ms: display.last_message_at,
        deleted_at_ms: display.deleted_at,
    })
}

fn map_member(member: mem_data::ThreadGroupMember) -> AppResult<ThreadGroupMemberView> {
    let group_id = positive_response_id("ThreadGroupMember group_id", member.group_id)?;
    let user_id = required_positive_response_id("ThreadGroupMember user_id", member.user_id)?;
    let thread_id = optional_positive_response_id("ThreadGroupMember thread_id", member.thread_id)?;
    if member.state == mem_data::ThreadGroupMemberState::Active as i32 && thread_id.is_none() {
        return Err(AppError::Config(
            "active ThreadGroup member did not contain a thread id".into(),
        ));
    }
    Ok(ThreadGroupMemberView {
        thread_canonical_key: member.thread_canonical_key,
        group_id,
        user_id,
        thread_id,
        source: member.source,
        role: enum_name(member.role, &["unspecified", "root", "member"]),
        state: enum_name(
            member.state,
            &["unspecified", "active", "deleted", "redirected"],
        ),
        provenance: enum_name(
            member.provenance,
            &["unspecified", "reconciler", "operator"],
        ),
        identity_scope: member.identity_scope,
        native_id: member.native_id,
        deleted_at_ms: member.deleted_at,
        display: member.display.map(map_display).transpose()?,
    })
}

fn map_relation(relation: mem_data::ThreadRelation) -> AppResult<ThreadRelationView> {
    Ok(ThreadRelationView {
        id: positive_response_id("ThreadRelation id", relation.id)?,
        parent_thread_id: optional_positive_response_id(
            "ThreadRelation parent_thread_id",
            relation.parent_thread_id,
        )?,
        child_thread_id: optional_positive_response_id(
            "ThreadRelation child_thread_id",
            relation.child_thread_id,
        )?,
        parent_thread_canonical_key: relation.parent_thread_canonical_key,
        child_thread_canonical_key: relation.child_thread_canonical_key,
        parent_user_id: required_positive_response_id(
            "ThreadRelation parent_user_id",
            relation.parent_user_id,
        )?,
        child_user_id: required_positive_response_id(
            "ThreadRelation child_user_id",
            relation.child_user_id,
        )?,
        relation_type: enum_name(
            relation.relation_type,
            &["unspecified", "delegated", "fork", "continuation"],
        ),
        state: enum_name(
            relation.state,
            &["unspecified", "active", "retracted", "superseded"],
        ),
        selection_basis: enum_name(
            relation.selection_basis,
            &[
                "unspecified",
                "source_exact",
                "source_strong",
                "operator_confirmation",
            ],
        ),
        source_confidence: relation.source_confidence.map(|confidence| {
            enum_name(
                confidence,
                &["unspecified", "exact", "strong", "heuristic", "unsupported"],
            )
        }),
        selected_observation_id: optional_positive_response_id(
            "ThreadRelation selected_observation_id",
            relation.selected_observation_id,
        )?,
        selected_operator_decision_id: optional_positive_response_id(
            "ThreadRelation selected_operator_decision_id",
            relation.selected_operator_decision_id,
        )?,
        parent_group_id: optional_positive_response_id(
            "ThreadRelation parent_group_id",
            relation.parent_group_id,
        )?,
        child_group_id: optional_positive_response_id(
            "ThreadRelation child_group_id",
            relation.child_group_id,
        )?,
        parent_display: relation.parent_display.map(map_display).transpose()?,
        child_display: relation.child_display.map(map_display).transpose()?,
    })
}

fn map_unresolved(
    candidate: mem_data::ThreadGroupCandidateAssociation,
) -> AppResult<ThreadGroupUnresolvedView> {
    Ok(ThreadGroupUnresolvedView {
        id: positive_response_id("ThreadGroup candidate id", candidate.id)?,
        subject_thread_id: optional_positive_response_id(
            "ThreadGroup candidate subject_thread_id",
            candidate.subject_thread_id,
        )?,
        subject_source: candidate.subject_source,
        subject_user_id: required_positive_response_id(
            "ThreadGroup candidate subject_user_id",
            candidate.subject_user_id,
        )?,
        candidate_group_id: optional_positive_response_id(
            "ThreadGroup candidate candidate_group_id",
            candidate.candidate_group_id,
        )?,
        candidate_parent_thread_id: optional_positive_response_id(
            "ThreadGroup candidate candidate_parent_thread_id",
            candidate.candidate_parent_thread_id,
        )?,
        state: enum_name(
            candidate.state,
            &[
                "unspecified",
                "pending",
                "candidate",
                "ambiguous",
                "conflict",
                "unsupported",
                "superseded",
            ],
        ),
        subject_identity_scope_known: candidate.subject_identity_scope_known,
        subject_identity_scope_value: candidate.subject_identity_scope_value,
        subject_native_id: candidate.subject_native_id,
        selected_observation_id: optional_positive_response_id(
            "ThreadGroup candidate selected_observation_id",
            candidate.selected_observation_id,
        )?,
    })
}

fn map_observation(observation: mem_data::ThreadObservation) -> AppResult<ThreadObservationView> {
    let subject_user_id = required_positive_response_id(
        "ThreadObservation subject_user_id",
        observation.subject_user_id,
    )?;
    let candidate_parent_user_id = match (
        observation.candidate_parent_present,
        observation.candidate_parent_user_id,
    ) {
        (false, None) => None,
        (false, Some(_)) => {
            return Err(AppError::Config(
                "ThreadObservation parent user_id must be absent when candidate_parent_present is false".into(),
            ));
        }
        (true, Some(parent_user_id)) => Some(positive_response_id(
            "ThreadObservation candidate_parent_user_id",
            parent_user_id,
        )?),
        (true, None) => {
            return Err(AppError::Config(
                "ThreadObservation candidate_parent_user_id is required when candidate_parent_present is true".into(),
            ));
        }
    };
    Ok(ThreadObservationView {
        id: positive_response_id("ThreadObservation id", observation.id)?,
        subject_source: observation.subject_source,
        subject_user_id,
        subject_identity_scope_known: observation.subject_identity_scope_known,
        subject_identity_scope_value: observation.subject_identity_scope_value,
        subject_native_id: observation.subject_native_id,
        candidate_parent_present: observation.candidate_parent_present,
        candidate_parent_user_id,
        candidate_parent_source: observation.candidate_parent_source,
        candidate_parent_identity_scope_known: observation.candidate_parent_identity_scope_known,
        candidate_parent_identity_scope_value: observation.candidate_parent_identity_scope_value,
        candidate_parent_native_id: observation.candidate_parent_native_id,
        relation_kind: observation.relation_kind,
        evidence_kind: enum_name(
            observation.evidence_kind,
            &[
                "unspecified",
                "source_event",
                "source_field",
                "negative_or_conflict",
                "derived_rejection",
                "derived_conflict",
            ],
        ),
        polarity: enum_name(
            observation.polarity,
            &["unspecified", "supports", "negates", "conflicts"],
        ),
        source_confidence: observation.source_confidence.map(|confidence| {
            enum_name(
                confidence,
                &["unspecified", "exact", "strong", "heuristic", "unsupported"],
            )
        }),
        state: enum_name(
            observation.state,
            &[
                "unspecified",
                "pending",
                "candidate",
                "selected",
                "conflict",
                "superseded",
                "unsupported",
            ],
        ),
        adapter_version: observation.adapter_version,
        source_record_ref: observation.source_record_ref,
        observed_at_ms: observation.observed_at,
    })
}

fn positive_response_id(field: &str, id: i64) -> AppResult<i64> {
    validate_positive_user_id(field, id)
}

fn required_positive_response_id(field: &str, id: Option<i64>) -> AppResult<i64> {
    let id = id.ok_or_else(|| AppError::Config(format!("{field} is required")))?;
    positive_response_id(field, id)
}

fn optional_positive_response_id(field: &str, id: Option<i64>) -> AppResult<Option<i64>> {
    id.map(|id| positive_response_id(field, id)).transpose()
}

fn map_summary(memory: mem_data::Memory) -> AppResult<ThreadGroupSummaryView> {
    let memory_id = required_positive_response_id(
        "ThreadGroup summary memory id",
        memory.id.map(|id| id.value),
    )?;
    let data = memory.data.ok_or_else(|| {
        AppError::Config("ThreadGroup summary response did not contain memory data".into())
    })?;
    let fields = data
        .metadata
        .as_deref()
        .and_then(parse_summary_metadata)
        .unwrap_or_default();
    let group_id = optional_positive_response_id("ThreadGroup summary group_id", fields.group_id)?;
    let passthrough_source_memory_id = optional_positive_response_id(
        "ThreadGroup summary passthrough_source_memory_id",
        fields.passthrough_source_memory_id,
    )?;

    Ok(ThreadGroupSummaryView {
        memory_id,
        external_id: data.external_id,
        content_json: data.content,
        metadata: data.metadata,
        updated_at_ms: data.updated_at,
        membership_snapshot_digest: fields.membership_snapshot_digest,
        group_id,
        active_member_count: fields.active_member_count,
        deleted_member_count: fields.deleted_member_count,
        unresolved_count: fields.unresolved_count,
        member_summary_count: fields.member_summary_count,
        summary_version: fields.summary_version,
        aggregation_mode: fields.aggregation_mode,
        passthrough_source_memory_id,
    })
}

#[derive(Debug, Default, serde::Deserialize)]
struct ThreadGroupSummaryMetadata {
    membership_snapshot_digest: Option<String>,
    #[serde(deserialize_with = "crate::serde_id::option::deserialize", default)]
    group_id: Option<i64>,
    active_member_count: Option<i64>,
    deleted_member_count: Option<i64>,
    unresolved_count: Option<i64>,
    member_summary_count: Option<i64>,
    summary_version: Option<String>,
    aggregation_mode: Option<String>,
    #[serde(deserialize_with = "crate::serde_id::option::deserialize", default)]
    passthrough_source_memory_id: Option<i64>,
}

fn parse_summary_metadata(raw: &str) -> Option<ThreadGroupSummaryMetadata> {
    serde_json::from_str(raw).ok()
}

fn enum_name(value: i32, names: &[&str]) -> String {
    names
        .get(value.max(0) as usize)
        .unwrap_or(&"unknown")
        .to_string()
}

fn group_display_name(group: &mem_data::ThreadGroup) -> String {
    group
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| {
            group
                .root_display
                .as_ref()
                .and_then(|display| display.description.as_deref())
                .filter(|description| !description.trim().is_empty())
        })
        .unwrap_or("無題のスレッドグループ")
        .to_string()
}

#[cfg(test)]
fn member_thread_id(member: &mem_data::ThreadGroupMember) -> Option<i64> {
    (member.state == mem_data::ThreadGroupMemberState::Active as i32).then_some(member.thread_id?)
}

#[cfg(test)]
fn ordered_member_ids(
    members: &[mem_data::ThreadGroupMember],
    relations: &[mem_data::ThreadRelation],
) -> Vec<i64> {
    let ids: Vec<i64> = members.iter().filter_map(member_thread_id).collect();
    let id_set: HashSet<i64> = ids.iter().copied().collect();
    let mut children = HashMap::<i64, Vec<i64>>::new();
    let mut child_ids = HashSet::new();
    for relation in relations
        .iter()
        .filter(|relation| relation.state == mem_data::ThreadRelationState::Active as i32)
    {
        let (Some(parent), Some(child)) = (relation.parent_thread_id, relation.child_thread_id)
        else {
            continue;
        };
        if id_set.contains(&parent) && id_set.contains(&child) {
            children.entry(parent).or_default().push(child);
            child_ids.insert(child);
        }
    }
    let mut ordered = Vec::with_capacity(ids.len());
    let mut visited = HashSet::new();
    for id in ids.iter().copied().filter(|id| !child_ids.contains(id)) {
        visit_member(id, &children, &mut visited, &mut ordered);
    }
    for id in ids {
        visit_member(id, &children, &mut visited, &mut ordered);
    }
    ordered
}

#[cfg(test)]
fn visit_member(
    id: i64,
    children: &HashMap<i64, Vec<i64>>,
    visited: &mut HashSet<i64>,
    ordered: &mut Vec<i64>,
) {
    if !visited.insert(id) {
        return;
    }
    ordered.push(id);
    if let Some(descendants) = children.get(&id) {
        for child in descendants {
            visit_member(*child, children, visited, ordered);
        }
    }
}

fn map_thread_group_status(status: tonic::Status) -> AppError {
    if is_rpc_unsupported(&status) {
        AppError::ThreadGroupUnsupported
    } else {
        AppError::Grpc(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::proto::llm_memory::data as mem_data;

    fn summary_memory(id: i64) -> mem_data::Memory {
        mem_data::Memory {
            id: Some(mem_data::MemoryId { value: id }),
            ..Default::default()
        }
    }

    fn entry(group_id: i64, summary: Option<i64>) -> mem_svc::ThreadGroupSummaryEntry {
        mem_svc::ThreadGroupSummaryEntry {
            group_id,
            summary: summary.map(summary_memory),
        }
    }

    #[test]
    fn summary_entries_are_aligned_with_the_requested_ids() {
        let summaries = align_summary_entries(
            &[3, 1, 3],
            vec![entry(3, Some(30)), entry(1, None), entry(3, Some(30))],
        )
        .unwrap();
        assert_eq!(
            summaries
                .iter()
                .map(|summary| summary
                    .as_ref()
                    .and_then(|memory| memory.id.map(|id| id.value)))
                .collect::<Vec<_>>(),
            vec![Some(30), None, Some(30)]
        );
    }

    #[test]
    fn misaligned_summary_entries_are_rejected() {
        // Wrong length.
        assert!(align_summary_entries(&[1, 2], vec![entry(1, None)]).is_err());
        // Same length but out of request order.
        assert!(align_summary_entries(&[1, 2], vec![entry(2, None), entry(1, None)]).is_err());
    }

    #[test]
    fn group_display_name_prefers_title_then_root_description_then_fallback() {
        let title = sample_group(Some("A title"), Some("A root"));
        assert_eq!(group_display_name(&title), "A title");

        let root = sample_group(None, Some("A root"));
        assert_eq!(group_display_name(&root), "A root");

        let empty = sample_group(None, None);
        assert_eq!(group_display_name(&empty), "無題のスレッドグループ");
    }

    #[test]
    fn deleted_member_has_no_thread_detail_target() {
        let deleted = mem_data::ThreadGroupMember {
            thread_id: None,
            state: mem_data::ThreadGroupMemberState::Deleted as i32,
            ..Default::default()
        };
        assert_eq!(member_thread_id(&deleted), None);

        let live = mem_data::ThreadGroupMember {
            thread_id: Some(42),
            state: mem_data::ThreadGroupMemberState::Active as i32,
            ..Default::default()
        };
        assert_eq!(member_thread_id(&live), Some(42));
    }

    #[test]
    fn lineage_members_are_ordered_by_active_relations_and_keep_orphans() {
        let members = vec![member(3, "child"), member(1, "root"), member(2, "orphan")];
        let relations = vec![mem_data::ThreadRelation {
            parent_thread_id: Some(1),
            child_thread_id: Some(3),
            state: mem_data::ThreadRelationState::Active as i32,
            ..Default::default()
        }];

        let ordered = ordered_member_ids(&members, &relations);
        assert_eq!(ordered, vec![1, 3, 2]);
    }

    #[test]
    fn unsupported_status_is_explicitly_classified() {
        assert!(is_rpc_unsupported(&tonic::Status::unimplemented("missing")));
        assert!(!is_rpc_unsupported(&tonic::Status::invalid_argument(
            "bad request"
        )));
    }

    #[test]
    fn group_summary_metadata_exposes_digest_and_detail_counts() {
        let metadata = parse_summary_metadata(
            r#"{"group_id":"7","membership_snapshot_digest":"digest-a","active_member_count":2,"deleted_member_count":1,"unresolved_count":0,"member_summary_count":2,"summary_version":"1.0","aggregation_mode":"passthrough","passthrough_source_memory_id":"9007199254740993"}"#,
        )
        .expect("valid group summary metadata");
        assert_eq!(metadata.group_id, Some(7));
        assert_eq!(
            metadata.membership_snapshot_digest.as_deref(),
            Some("digest-a")
        );
        assert_eq!(metadata.member_summary_count, Some(2));
        assert_eq!(metadata.summary_version.as_deref(), Some("1.0"));
        assert_eq!(metadata.aggregation_mode.as_deref(), Some("passthrough"));
        assert_eq!(
            metadata.passthrough_source_memory_id,
            Some(9_007_199_254_740_993)
        );
    }

    #[test]
    fn group_summary_maps_metadata_fields_and_serializes_source_memory_id_as_string() {
        let summary = map_summary(mem_data::Memory {
            id: Some(mem_data::MemoryId {
                value: 42,
            }),
            data: Some(mem_data::MemoryData {
                content: "summary content".into(),
                metadata: Some(
                    r#"{"aggregation_mode":"passthrough","passthrough_source_memory_id":"9007199254740993"}"#
                        .into(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        })
        .expect("summary with memory data");

        assert_eq!(summary.aggregation_mode.as_deref(), Some("passthrough"));
        assert_eq!(
            summary.passthrough_source_memory_id,
            Some(9_007_199_254_740_993)
        );
        let json = serde_json::to_value(summary).expect("serializable summary");
        assert_eq!(
            json["passthrough_source_memory_id"],
            serde_json::json!("9007199254740993")
        );
    }

    #[test]
    fn group_summary_rejects_missing_or_nonpositive_response_ids() {
        for memory in [
            mem_data::Memory {
                id: None,
                data: Some(mem_data::MemoryData {
                    content: "summary content".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            mem_data::Memory {
                id: Some(mem_data::MemoryId { value: 0 }),
                data: Some(mem_data::MemoryData {
                    content: "summary content".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            mem_data::Memory {
                id: Some(mem_data::MemoryId { value: 42 }),
                data: None,
                ..Default::default()
            },
            mem_data::Memory {
                id: Some(mem_data::MemoryId { value: 42 }),
                data: Some(mem_data::MemoryData {
                    content: "summary content".into(),
                    metadata: Some(r#"{"group_id":"0"}"#.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ] {
            assert!(map_summary(memory).is_err());
        }
    }

    #[test]
    fn group_summary_keeps_backward_compatibility_for_missing_or_invalid_metadata() {
        for (metadata, expected_summary_version) in [
            (None, None),
            (Some("not json".to_string()), None),
            (
                Some(r#"{"summary_version":"legacy"}"#.to_string()),
                Some("legacy"),
            ),
        ] {
            let summary = map_summary(mem_data::Memory {
                id: Some(mem_data::MemoryId { value: 42 }),
                data: Some(mem_data::MemoryData {
                    content: "summary content".into(),
                    metadata,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .expect("summary with memory data");

            assert_eq!(summary.memory_id, 42);
            assert_eq!(summary.content_json, "summary content");
            assert_eq!(summary.summary_version.as_deref(), expected_summary_version);
            assert_eq!(summary.aggregation_mode, None);
            assert_eq!(summary.passthrough_source_memory_id, None);
        }
    }

    #[test]
    fn group_search_request_maps_memory_filters_and_cursor() {
        let request = SearchThreadGroupsRequest {
            query_text: "rust async".to_string(),
            mode: super::super::search::SearchMode::Keyword,
            group_owner_user_id: Some(7),
            member_user_id: Some(1),
            user_id: None,
            created_after_ms: Some(10),
            created_before_ms: Some(20),
            labels_any: vec!["project:lookback".to_string()],
            label_match: Some(LabelMatch::All),
            allow_cross_member: false,
            page_size: Some(7),
            page_token: Some("opaque-token".to_string()),
        };

        let proto = build_group_search_request(&request, Vec::new(), 7);
        assert_eq!(proto.user_id, Some(7));
        assert!(!proto.allow_cross_member);
        assert_eq!(proto.page_size, Some(7));
        assert_eq!(proto.page_token.as_deref(), Some("opaque-token"));
        let predicate = &proto.predicates[0];
        assert_eq!(
            predicate.target,
            mem_svc::ThreadGroupSearchTarget::Memory as i32
        );
        assert_eq!(predicate.query_text, "rust async");
        let filter = predicate.memory_filter.as_ref().expect("memory filter");
        assert_eq!(filter.user_id, Some(1));
        assert_eq!(filter.created_after, Some(10));
        assert_eq!(filter.created_before, Some(20));
        let thread_filter = filter.thread_filter.as_ref().expect("thread filter");
        assert_eq!(thread_filter.labels, vec!["project:lookback"]);
        assert_eq!(
            thread_filter.label_match_mode,
            Some(LabelMatch::All.to_proto())
        );
    }

    #[test]
    fn group_owner_id_is_preserved_in_the_ui_read_model() {
        let group = map_group(mem_data::ThreadGroup {
            id: Some(mem_data::ThreadGroupId {
                value: 7,
                user_id: Some(7),
            }),
            user_id: Some(7),
            ..Default::default()
        })
        .expect("group owner is present");
        assert_eq!(group.user_id, 7);
    }

    #[test]
    fn response_mappers_reject_missing_and_nonpositive_owner_ids() {
        assert!(map_group(mem_data::ThreadGroup::default()).is_err());
        assert!(
            map_group(mem_data::ThreadGroup {
                id: Some(mem_data::ThreadGroupId {
                    value: 7,
                    user_id: None,
                }),
                user_id: Some(0),
                ..Default::default()
            })
            .is_err()
        );

        assert!(
            map_member(mem_data::ThreadGroupMember {
                group_id: 7,
                thread_id: None,
                state: mem_data::ThreadGroupMemberState::Deleted as i32,
                user_id: None,
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            map_relation(mem_data::ThreadRelation {
                id: 1,
                parent_user_id: Some(1),
                child_user_id: Some(0),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            map_unresolved(mem_data::ThreadGroupCandidateAssociation {
                id: 1,
                subject_user_id: None,
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn deleted_thread_placeholder_keeps_its_required_member_owner() {
        let deleted = map_member(mem_data::ThreadGroupMember {
            group_id: 7,
            thread_id: None,
            user_id: Some(2),
            state: mem_data::ThreadGroupMemberState::Deleted as i32,
            ..Default::default()
        })
        .expect("deleted member retains its owner");

        assert_eq!(deleted.user_id, 2);
        assert_eq!(deleted.thread_id, None);
    }

    #[test]
    fn observation_parent_owner_presence_matches_candidate_parent_presence() {
        let mut observation = mem_data::ThreadObservation {
            id: 1,
            subject_user_id: Some(2),
            candidate_parent_present: false,
            ..Default::default()
        };
        assert_eq!(
            map_observation(observation.clone())
                .unwrap()
                .candidate_parent_user_id,
            None
        );

        observation.candidate_parent_user_id = Some(1);
        assert!(map_observation(observation.clone()).is_err());

        observation.candidate_parent_present = true;
        observation.candidate_parent_user_id = None;
        assert!(map_observation(observation.clone()).is_err());

        observation.candidate_parent_user_id = Some(1);
        assert_eq!(
            map_observation(observation)
                .unwrap()
                .candidate_parent_user_id,
            Some(1)
        );
    }

    #[test]
    fn lineage_mapping_keeps_all_mixed_owner_members_relations_and_candidate_ids() {
        let lineage = map_lineage(mem_data::ThreadGroupLineage {
            group: Some(mem_data::ThreadGroup {
                id: Some(mem_data::ThreadGroupId {
                    value: 70,
                    user_id: Some(7),
                }),
                user_id: Some(7),
                membership_snapshot_digest: "snapshot-7".into(),
                ..Default::default()
            }),
            members: vec![
                mem_data::ThreadGroupMember {
                    group_id: 70,
                    thread_id: Some(11),
                    user_id: Some(1),
                    owner_scope: "user:1".into(),
                    state: mem_data::ThreadGroupMemberState::Active as i32,
                    ..Default::default()
                },
                mem_data::ThreadGroupMember {
                    group_id: 70,
                    thread_id: Some(12),
                    user_id: Some(2),
                    owner_scope: "user:2".into(),
                    state: mem_data::ThreadGroupMemberState::Active as i32,
                    ..Default::default()
                },
                mem_data::ThreadGroupMember {
                    group_id: 70,
                    thread_id: None,
                    user_id: Some(2),
                    owner_scope: "user:2".into(),
                    state: mem_data::ThreadGroupMemberState::Deleted as i32,
                    ..Default::default()
                },
            ],
            relations: vec![mem_data::ThreadRelation {
                id: 30,
                parent_thread_id: Some(11),
                child_thread_id: Some(12),
                parent_user_id: Some(1),
                child_user_id: Some(2),
                ..Default::default()
            }],
            unresolved: vec![mem_data::ThreadGroupCandidateAssociation {
                id: 40,
                subject_thread_id: Some(12),
                subject_user_id: Some(2),
                subject_owner_scope: "user:2".into(),
                ..Default::default()
            }],
            observations: vec![mem_data::ThreadObservation {
                id: 50,
                subject_user_id: Some(2),
                subject_owner_scope: "user:2".into(),
                candidate_parent_present: true,
                candidate_parent_user_id: Some(1),
                candidate_parent_owner_scope: Some("user:1".into()),
                ..Default::default()
            }],
        })
        .expect("typed owner fields from the full lineage are valid");

        assert_eq!(lineage.group.user_id, 7);
        assert_eq!(lineage.group.membership_snapshot_digest, "snapshot-7");
        assert_eq!(lineage.members.len(), 3);
        assert_eq!(
            lineage
                .members
                .iter()
                .map(|member| member.user_id)
                .collect::<Vec<_>>(),
            vec![1, 2, 2]
        );
        assert_eq!(lineage.members[2].thread_id, None);
        assert_eq!(lineage.relations.len(), 1);
        assert_eq!(lineage.relations[0].parent_user_id, 1);
        assert_eq!(lineage.relations[0].child_user_id, 2);
        assert_eq!(lineage.unresolved[0].subject_user_id, 2);
        assert_eq!(lineage.observations[0].subject_user_id, 2);
        assert_eq!(lineage.observations[0].candidate_parent_user_id, Some(1));

        let json = serde_json::to_value(lineage).unwrap();
        assert!(json.to_string().contains("snapshot-7"));
        for legacy_field in [
            "owner_scope",
            "subject_owner_scope",
            "candidate_parent_owner_scope",
        ] {
            assert!(json.get(legacy_field).is_none());
            assert!(!json.to_string().contains(legacy_field));
        }
    }

    #[test]
    fn request_owner_resolution_defaults_only_at_rust_boundary_and_checks_aliases() {
        use super::super::thread_group_owner::resolve_group_owner_user_id;

        assert_eq!(resolve_group_owner_user_id(None, None).unwrap(), 1);
        assert_eq!(resolve_group_owner_user_id(Some(7), None).unwrap(), 7);
        assert_eq!(resolve_group_owner_user_id(None, Some(2)).unwrap(), 2);
        assert_eq!(resolve_group_owner_user_id(Some(7), Some(7)).unwrap(), 7);
        assert!(resolve_group_owner_user_id(Some(7), Some(1)).is_err());
        for invalid in [0, -1] {
            assert!(resolve_group_owner_user_id(Some(invalid), None).is_err());
            assert!(resolve_group_owner_user_id(None, Some(invalid)).is_err());
        }
    }

    #[test]
    fn count_commands_resolve_legacy_owner_and_reject_invalid_aliases() {
        assert_eq!(resolve_count_group_owner_user_id(None, Some(7)).unwrap(), 7);
        assert_eq!(
            resolve_count_group_owner_user_id(Some(7), Some(7)).unwrap(),
            7
        );
        assert!(resolve_count_group_owner_user_id(Some(0), None).is_err());
        assert!(resolve_count_group_owner_user_id(None, Some(0)).is_err());
        assert!(resolve_count_group_owner_user_id(None, Some(-1)).is_err());
        assert!(resolve_count_group_owner_user_id(Some(7), Some(1)).is_err());
    }

    #[test]
    fn group_search_request_keeps_semantic_vector_and_explicit_cross_member() {
        let request = SearchThreadGroupsRequest {
            query_text: "semantic".to_string(),
            mode: super::super::search::SearchMode::Semantic,
            group_owner_user_id: None,
            member_user_id: None,
            user_id: None,
            created_after_ms: None,
            created_before_ms: None,
            labels_any: Vec::new(),
            label_match: None,
            allow_cross_member: true,
            page_size: None,
            page_token: None,
        };

        let proto = build_group_search_request(&request, vec![0.25, 0.75], 1);
        assert!(proto.allow_cross_member);
        assert_eq!(
            proto.predicates[0].query_vectors[0].values,
            vec![0.25, 0.75]
        );
        assert_eq!(
            proto.predicates[0].mode,
            mem_svc::ThreadGroupSearchMode::Semantic as i32
        );
        assert!(
            proto.predicates[0]
                .memory_filter
                .as_ref()
                .expect("memory filter")
                .thread_filter
                .is_none()
        );
    }

    #[test]
    fn group_browse_request_uses_cursor_and_thread_filter_without_text_predicates() {
        let request = SearchThreadGroupsRequest {
            query_text: "".into(),
            mode: SearchMode::Keyword,
            group_owner_user_id: Some(1),
            member_user_id: None,
            user_id: Some(1),
            created_after_ms: Some(10),
            created_before_ms: Some(20),
            labels_any: vec!["project:lookback".into()],
            label_match: Some(LabelMatch::All),
            allow_cross_member: false,
            page_size: Some(25),
            page_token: Some("cursor".into()),
        };
        let proto = build_group_search_request(&request, Vec::new(), 1);
        assert!(proto.predicates.is_empty());
        assert_eq!(proto.page_token.as_deref(), Some("cursor"));
        let filter = proto.browse_filter.as_ref().expect("browse filter");
        assert_eq!(filter.labels, vec!["project:lookback"]);
        assert_eq!(filter.thread_created_after, Some(10));
        assert_eq!(filter.thread_created_before, Some(20));
        assert_eq!(filter.user_id, None);
    }

    #[test]
    fn group_owner_one_can_search_member_two_without_filtering_out_that_member() {
        let request = SearchThreadGroupsRequest {
            query_text: "member two content".into(),
            mode: SearchMode::Keyword,
            group_owner_user_id: Some(1),
            member_user_id: Some(2),
            user_id: None,
            created_after_ms: None,
            created_before_ms: None,
            labels_any: Vec::new(),
            label_match: None,
            allow_cross_member: false,
            page_size: None,
            page_token: None,
        };

        let proto = build_group_search_request(&request, Vec::new(), 1);
        assert_eq!(proto.user_id, Some(1), "select groups owned by user 1");
        let member_filter = proto.predicates[0]
            .memory_filter
            .as_ref()
            .expect("member memory filter");
        assert_eq!(member_filter.user_id, Some(2));
        assert_eq!(
            member_filter
                .thread_filter
                .as_ref()
                .expect("member thread filter")
                .user_id,
            Some(2)
        );
    }

    #[test]
    fn active_group_count_request_is_owner_scoped() {
        let request = active_group_list_request(7);
        assert_eq!(request.user_id, Some(7));
        assert!(!request.include_inactive);
    }

    #[test]
    fn malformed_group_search_result_is_rejected_instead_of_becoming_a_fake_group() {
        let error = map_search_result(mem_svc::ThreadGroupSearchResult::default())
            .expect_err("group is required by the search response contract");
        assert!(
            matches!(error, AppError::Config(message) if message.contains("did not contain a group"))
        );
    }

    fn sample_group(title: Option<&str>, description: Option<&str>) -> mem_data::ThreadGroup {
        mem_data::ThreadGroup {
            title: title.map(str::to_string),
            root_display: description.map(|description| mem_data::ThreadGroupThreadDisplay {
                description: Some(description.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn member(id: i64, key: &str) -> mem_data::ThreadGroupMember {
        mem_data::ThreadGroupMember {
            thread_id: Some(id),
            thread_canonical_key: key.to_string(),
            state: mem_data::ThreadGroupMemberState::Active as i32,
            ..Default::default()
        }
    }
}
