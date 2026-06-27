//! Tauri commands backing the Threads page (FR-THREAD-1/2/3).

use serde::{Deserialize, Serialize};
use tauri::State;
use tokio_stream::StreamExt;
use tracing::{debug, warn};

use crate::error::AppResult;
use crate::grpc::proto::llm_memory::data as mem_data;
use crate::grpc::proto::llm_memory::service as mem_svc;
use crate::grpc::proto::llm_memory::service::memory_service_client::MemoryServiceClient;
use crate::grpc::proto::llm_memory::service::thread_service_client::ThreadServiceClient;

use super::AppState;

/// One thread shaped for the frontend.
///
/// Mirrors `llm_memory.data.Thread` but flattens optional nested ids to
/// plain numbers so the TS side doesn't need to handle protobuf wrapper
/// types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummary {
    #[serde(with = "crate::serde_id")]
    pub id: i64,
    #[serde(with = "crate::serde_id")]
    pub user_id: i64,
    pub description: Option<String>,
    pub channel: Option<String>,
    pub labels: Vec<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LabelMatch {
    Any,
    All,
}

impl LabelMatch {
    pub(crate) fn to_proto(self) -> i32 {
        match self {
            LabelMatch::Any => mem_data::LabelMatchMode::LabelAny as i32,
            LabelMatch::All => mem_data::LabelMatchMode::LabelAll as i32,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListThreadsRequest {
    pub user_id: Option<i64>,
    pub limit: Option<i32>,
    pub offset: Option<i64>,
    pub created_after_ms: Option<i64>,
    pub created_before_ms: Option<i64>,
    // serde rejects missing `Vec` fields by default; the frontend omits
    // this when no labels are picked, so accept absent as "no filter".
    #[serde(default)]
    pub labels_any: Vec<String>,
    #[serde(default)]
    pub label_match: Option<LabelMatch>,
}

#[tauri::command]
pub async fn list_threads(
    state: State<'_, AppState>,
    req: ListThreadsRequest,
) -> AppResult<Vec<ThreadSummary>> {
    let mut client = ThreadServiceClient::new(state.memories_channel().await?);

    let user_id = req.user_id.unwrap_or(1);
    let match_mode = req.label_match.unwrap_or(LabelMatch::Any);

    debug!(user_id, limit = ?req.limit, "list_threads request");

    // The label filter RPC is separate so an over-limit selection isn't truncated.
    let mut stream = if req.labels_any.is_empty() {
        let request = mem_svc::FindThreadListByUserIdRequest {
            user_id: Some(mem_data::UserId { value: user_id }),
            limit: req.limit,
            offset: req.offset,
            created_after: req.created_after_ms,
            created_before: req.created_before_ms,
            updated_after: None,
            updated_before: None,
            sort: None,
        };
        client
            .find_thread_list_by_user_id(request)
            .await?
            .into_inner()
    } else {
        let request = mem_svc::FindThreadListByLabelsRequest {
            labels: req.labels_any.clone(),
            match_mode: Some(match_mode.to_proto()),
            limit: req.limit,
            offset: req.offset,
            user_id: Some(user_id),
            created_after: req.created_after_ms,
            created_before: req.created_before_ms,
            updated_after: None,
            updated_before: None,
            sort: None,
        };
        client
            .find_thread_list_by_labels(request)
            .await?
            .into_inner()
    };

    let mut received = 0usize;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        received += 1;
        let thread = item?;
        match thread_to_summary(thread) {
            Some(summary) => out.push(summary),
            None => warn!("thread row had a None field; skipping"),
        }
    }
    debug!(received, returned = out.len(), "list_threads done");
    Ok(out)
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FindDistinctLabelsRequest {
    pub user_id: Option<i64>,
    pub limit: Option<i32>,
    pub offset: Option<i64>,
    pub created_after_ms: Option<i64>,
    pub created_before_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LabelWithCount {
    pub label: String,
    pub thread_count: i64,
}

impl From<mem_svc::LabelWithCount> for LabelWithCount {
    fn from(l: mem_svc::LabelWithCount) -> Self {
        Self {
            label: l.label,
            thread_count: l.thread_count,
        }
    }
}

#[tauri::command]
pub async fn find_distinct_labels(
    state: State<'_, AppState>,
    req: FindDistinctLabelsRequest,
) -> AppResult<Vec<LabelWithCount>> {
    let mut client = ThreadServiceClient::new(state.memories_channel().await?);
    let user_id = req.user_id.unwrap_or(1);

    let request = mem_svc::FindDistinctLabelsRequest {
        user_id: Some(user_id),
        limit: req.limit,
        offset: req.offset,
        created_after: req.created_after_ms,
        created_before: req.created_before_ms,
        updated_after: None,
        updated_before: None,
    };

    let resp = client.find_distinct_labels(request).await?.into_inner();
    Ok(resp.labels.into_iter().map(Into::into).collect())
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FindCoOccurringLabelsRequest {
    pub user_id: Option<i64>,
    pub labels: Vec<String>,
    pub limit: Option<i32>,
    pub offset: Option<i64>,
    pub created_after_ms: Option<i64>,
    pub created_before_ms: Option<i64>,
}

#[tauri::command]
pub async fn find_co_occurring_labels(
    state: State<'_, AppState>,
    req: FindCoOccurringLabelsRequest,
) -> AppResult<Vec<LabelWithCount>> {
    let mut client = ThreadServiceClient::new(state.memories_channel().await?);
    let user_id = req.user_id.unwrap_or(1);

    let request = mem_svc::FindCoOccurringLabelsRequest {
        labels: req.labels,
        user_id: Some(user_id),
        limit: req.limit,
        offset: req.offset,
        created_after: req.created_after_ms,
        created_before: req.created_before_ms,
        updated_after: None,
        updated_before: None,
    };

    let resp = client.find_co_occurring_labels(request).await?.into_inner();
    Ok(resp.labels.into_iter().map(Into::into).collect())
}

#[derive(Debug, Clone, Deserialize)]
pub struct FindMemoriesRequest {
    #[serde(with = "crate::serde_id")]
    pub thread_id: i64,
    pub limit: Option<i32>,
    pub offset: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryRow {
    #[serde(with = "crate::serde_id")]
    pub id: i64,
    pub role: i32,
    pub content_type: i32,
    pub content: String,
    pub created_at_ms: i64,
    pub metadata: Option<String>,
    pub external_id: Option<String>,
}

#[tauri::command]
pub async fn find_memories_by_thread_id(
    state: State<'_, AppState>,
    req: FindMemoriesRequest,
) -> AppResult<Vec<MemoryRow>> {
    let mut client = ThreadServiceClient::new(state.memories_channel().await?);

    let request = mem_svc::FindMemoriesByThreadIdRequest {
        thread_id: Some(mem_data::ThreadId {
            value: req.thread_id,
        }),
        limit: req.limit,
        offset: req.offset,
        roles: vec![],
        content_types: vec![],
    };

    let mut stream = client
        .find_memories_by_thread_id(request)
        .await?
        .into_inner();
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let memory = item?;
        if let Some(row) = memory_to_row(memory) {
            out.push(row);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Deserialize)]
pub struct FindMemoryPositionRequest {
    #[serde(with = "crate::serde_id")]
    pub thread_id: i64,
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FindMemoryThreadPositionRequest {
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemoryPosition {
    /// Zero-based index of the memory within the thread's `position`-ordered
    /// list — the same axis `ThreadHit.top_position` rides, so it seeds the
    /// existing ThreadDetail scroll-to-hit machinery.
    pub position: i32,
    /// Total memory count in the thread; caps downward paging in the UI.
    pub thread_total: i32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemoryThreadPosition {
    /// First thread membership hydrated on the Memory row. Shared memories can
    /// belong to multiple threads; the UI only needs a stable place to open.
    #[serde(with = "crate::serde_id")]
    pub thread_id: i64,
    pub position: i32,
    pub thread_total: i32,
}

/// Resolve a memory's thread-internal position so a `memory_ids` cross-link
/// (personality signals) can scroll to it. `FindMemoriesByThreadId` streams
/// in `thread_memory.position` order, so the running index of the target —
/// counted over the same rows ThreadDetail renders (None rows skipped) — is
/// the position. Returns `None` when the memory isn't in the thread.
#[tauri::command]
pub async fn find_memory_position(
    state: State<'_, AppState>,
    req: FindMemoryPositionRequest,
) -> AppResult<Option<MemoryPosition>> {
    find_memory_position_in_thread(
        state.memories_channel().await?,
        req.thread_id,
        req.memory_id,
    )
    .await
}

/// Resolve a memory to one hydrated thread membership and its position in
/// that thread. Profile-level personality refs only carry `memory_ids`, not
/// `(thread_id, memory_id)` pairs, so this path relies on memories' output-only
/// `MemoryData.thread_ids` enrichment instead of guessing from profile JSON.
#[tauri::command]
pub async fn find_memory_thread_position(
    state: State<'_, AppState>,
    req: FindMemoryThreadPositionRequest,
) -> AppResult<Option<MemoryThreadPosition>> {
    let channel = state.memories_channel().await?;
    let mut memory_client = MemoryServiceClient::new(channel.clone());
    let Some(memory) = memory_client
        .find(mem_data::MemoryId {
            value: req.memory_id,
        })
        .await?
        .into_inner()
        .data
    else {
        return Ok(None);
    };
    let Some(thread_id) = first_thread_id(&memory) else {
        return Ok(None);
    };
    let Some(pos) = find_memory_position_in_thread(channel, thread_id, req.memory_id).await? else {
        return Ok(None);
    };
    Ok(Some(MemoryThreadPosition {
        thread_id,
        position: pos.position,
        thread_total: pos.thread_total,
    }))
}

async fn find_memory_position_in_thread(
    channel: tonic::transport::Channel,
    thread_id: i64,
    memory_id: i64,
) -> AppResult<Option<MemoryPosition>> {
    let mut client = ThreadServiceClient::new(channel);

    let request = mem_svc::FindMemoriesByThreadIdRequest {
        thread_id: Some(mem_data::ThreadId { value: thread_id }),
        limit: None,
        offset: None,
        roles: vec![],
        content_types: vec![],
    };

    let mut stream = client
        .find_memories_by_thread_id(request)
        .await?
        .into_inner();
    let mut ids: Vec<i64> = Vec::new();
    while let Some(item) = stream.next().await {
        if let Some(row) = memory_to_row(item?) {
            ids.push(row.id);
        }
    }
    Ok(compute_position(&ids, memory_id))
}

fn first_thread_id(memory: &mem_data::Memory) -> Option<i64> {
    memory.data.as_ref()?.thread_ids.first().map(|id| id.value)
}

/// Find `target`'s index within the position-ordered id list and the list
/// length. Pure so the index/total arithmetic is unit-tested without gRPC.
fn compute_position(ids: &[i64], target: i64) -> Option<MemoryPosition> {
    let thread_total = ids.len() as i32;
    ids.iter()
        .position(|&id| id == target)
        .map(|i| MemoryPosition {
            position: i as i32,
            thread_total,
        })
}

#[tauri::command]
pub async fn count_threads(state: State<'_, AppState>) -> AppResult<i64> {
    let mut client = ThreadServiceClient::new(state.memories_channel().await?);
    let resp = client.count(mem_svc::FindCondition {}).await?;
    Ok(resp.into_inner().total)
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteThreadRequest {
    #[serde(with = "crate::serde_id")]
    pub thread_id: i64,
}

/// Delete a thread and, by the server's cascade, every Memory attached to it
/// (`/llm_memory.service.ThreadService/Delete`). Irreversible — the UI gates
/// this behind a confirm dialog inside the thread-detail modal.
#[tauri::command]
pub async fn delete_thread(state: State<'_, AppState>, req: DeleteThreadRequest) -> AppResult<()> {
    let mut client = ThreadServiceClient::new(state.memories_channel().await?);
    client
        .delete(mem_data::ThreadId {
            value: req.thread_id,
        })
        .await?;
    Ok(())
}

fn thread_to_summary(t: mem_data::Thread) -> Option<ThreadSummary> {
    let data = t.data?;
    Some(ThreadSummary {
        id: t.id?.value,
        user_id: data.user_id?.value,
        description: data.description,
        channel: data.channel,
        labels: data.labels,
        created_at_ms: data.created_at,
        updated_at_ms: data.updated_at,
    })
}

fn memory_to_row(m: mem_data::Memory) -> Option<MemoryRow> {
    let data = m.data?;
    Some(MemoryRow {
        id: m.id?.value,
        role: data.role,
        content_type: data.content_type,
        content: data.content,
        created_at_ms: data.created_at,
        metadata: data.metadata,
        external_id: data.external_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_thread_request_deserializes_id_from_json_string() {
        // The frontend sends i64 ids as JSON strings (snowflakes exceed
        // Number.MAX_SAFE_INTEGER); serde_id must round-trip them to i64.
        let req: DeleteThreadRequest =
            serde_json::from_str(r#"{"thread_id":"9007199254740993"}"#).unwrap();
        assert_eq!(req.thread_id, 9_007_199_254_740_993);
    }

    #[test]
    fn compute_position_finds_index_and_total() {
        let ids = [10_i64, 20, 30, 40];
        assert_eq!(
            compute_position(&ids, 30),
            Some(MemoryPosition {
                position: 2,
                thread_total: 4,
            })
        );
    }

    #[test]
    fn compute_position_returns_none_when_absent() {
        assert_eq!(compute_position(&[1, 2, 3], 99), None);
    }

    #[test]
    fn compute_position_handles_empty_thread() {
        assert_eq!(compute_position(&[], 1), None);
    }

    #[test]
    fn first_thread_id_returns_first_hydrated_thread() {
        let memory = mem_data::Memory {
            data: Some(mem_data::MemoryData {
                thread_ids: vec![
                    mem_data::ThreadId { value: 10 },
                    mem_data::ThreadId { value: 20 },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(first_thread_id(&memory), Some(10));
    }

    #[test]
    fn first_thread_id_returns_none_without_hydrated_threads() {
        let memory = mem_data::Memory {
            data: Some(mem_data::MemoryData {
                thread_ids: vec![],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(first_thread_id(&memory), None);
        assert_eq!(first_thread_id(&mem_data::Memory::default()), None);
    }

    #[test]
    fn memory_to_row_carries_metadata_and_external_id() {
        let row = memory_to_row(mem_data::Memory {
            id: Some(mem_data::MemoryId { value: 42 }),
            data: Some(mem_data::MemoryData {
                content: "hello".into(),
                metadata: Some(r#"{"source":"codex"}"#.into()),
                external_id: Some("codex:line:10".into()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(row.metadata.as_deref(), Some(r#"{"source":"codex"}"#));
        assert_eq!(row.external_id.as_deref(), Some("codex:line:10"));
    }

    #[test]
    fn list_threads_request_deserializes_label_match_any_and_all() {
        let any: ListThreadsRequest =
            serde_json::from_str(r#"{"labels_any":["x"],"label_match":"any"}"#).unwrap();
        assert_eq!(any.label_match, Some(LabelMatch::Any));
        let all: ListThreadsRequest =
            serde_json::from_str(r#"{"labels_any":["x"],"label_match":"all"}"#).unwrap();
        assert_eq!(all.label_match, Some(LabelMatch::All));
    }

    #[test]
    fn label_match_to_proto_maps_to_label_match_mode_enum() {
        assert_eq!(
            LabelMatch::Any.to_proto(),
            mem_data::LabelMatchMode::LabelAny as i32
        );
        assert_eq!(
            LabelMatch::All.to_proto(),
            mem_data::LabelMatchMode::LabelAll as i32
        );
    }

    #[test]
    fn find_co_occurring_labels_request_deserializes_labels_array() {
        let req: FindCoOccurringLabelsRequest =
            serde_json::from_str(r#"{"labels":["a","b"]}"#).unwrap();
        assert_eq!(req.labels, vec!["a".to_string(), "b".to_string()]);
        assert!(req.user_id.is_none());
    }
}
