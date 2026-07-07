//! Tauri commands backing the Personality tab.
//!
//! The MVP surfaces both layers of the personality model:
//! - layer 2: the merged profile stored as a memory owned by
//!   `personality_user_id=200000` with
//!   `external_id = "personality_profile:<user_id>"` (`get_personality`).
//! - layer 1: per-thread signals (`external_id = "personality:<thread_id>"`,
//!   owned by the same `personality_user_id`) listed by
//!   `list_personality_signals` — the evidence the merge actually consumed,
//!   surfaced when the user drills into the persona-stats "Signals" count.
//!
//! `thread_count` and `signal_count` are bundled in the `get_personality`
//! response so the persona-stats header (Threads / Signals / Categories /
//! Profile version) renders without extra IPC round-trips. `thread_count`
//! is derived via `FindThreadListByUserId` with a high limit because
//! `ThreadService.Count` carries no user_id filter (proto has an empty
//! `FindCondition` shape). `signal_count` is the number of
//! `personality_signal`-tagged threads — exactly the row count of
//! `list_personality_signals` — so the badge and the drawer never disagree.

use serde::{Deserialize, Serialize};
use tauri::State;
use tokio_stream::StreamExt;

use crate::error::AppResult;
use crate::grpc::proto::llm_memory::data as mem_data;
use crate::grpc::proto::llm_memory::service as mem_svc;
use crate::grpc::proto::llm_memory::service::memory_service_client::MemoryServiceClient;
use crate::grpc::proto::llm_memory::service::thread_service_client::ThreadServiceClient;

use super::AppState;

const PERSONALITY_USER_ID: i64 = 200_000;
const PROFILE_EXTERNAL_ID_PREFIX: &str = "personality_profile:";
const SIGNAL_EXTERNAL_ID_PREFIX: &str = "personality:";
/// Labels that thread-personality-single applies to a layer-1 personality
/// memory thread. `personality_signal` is added only when a signal whose
/// `no_signal == false` is stored, so filtering on all three mirrors
/// exactly the evidence the merged profile was built from.
const PERSONALITY_THREAD_LABEL: &str = "personality";
const SIGNAL_THREAD_LABEL: &str = "personality_signal";
/// Upper bound for the thread-count fallback. Threads count beyond this
/// is surfaced as "10000+" in the UI; 587 imported threads today and a
/// growth curve much smaller than this cap make it safe for the MVP.
const THREAD_COUNT_LIMIT: i32 = 10_000;
/// Upper bound for the signal-thread listing. Layer-1 signal threads are a
/// strict subset of source threads, so the same cap is comfortably safe.
const SIGNAL_THREAD_LIMIT: i32 = 10_000;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PersonalityProfile {
    /// Backing Memory row id, surfaced so the UI can delete the merged profile
    /// via `MemoryService.Delete`.
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
    pub content_json: String,
    pub updated_at_ms: i64,
    pub external_id: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PersonalityResponse {
    /// `None` when no merged profile row exists yet.
    pub profile: Option<PersonalityProfile>,
    /// Persona-stats "Threads" count for the user.
    pub thread_count: i64,
    /// True when the count is clamped by `THREAD_COUNT_LIMIT`; the UI
    /// renders `"<n>+"` instead of an exact number.
    pub thread_count_truncated: bool,
    /// Persona-stats "Signals" count: number of `personality_signal`-tagged
    /// threads (= `list_personality_signals` row count). The merged profile
    /// is built from exactly these threads.
    pub signal_count: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PersonalitySignal {
    /// Backing Memory row id, surfaced so the UI can delete this single signal
    /// via `MemoryService.Delete`.
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
    /// The *source* conversation thread (source-user space), not the
    /// personality-user thread the signal memory itself lives in. The drawer
    /// links here so the user lands on the original conversation.
    #[serde(with = "crate::serde_id")]
    pub source_thread_id: i64,
    pub content_json: String,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct GetPersonalityRequest {
    pub user_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListPersonalitySignalsRequest {
    pub user_id: Option<i64>,
}

#[tauri::command]
pub async fn get_personality(
    state: State<'_, AppState>,
    req: GetPersonalityRequest,
) -> AppResult<PersonalityResponse> {
    let channel = state.memories_channel().await?;
    let user_id = req.user_id.unwrap_or(1);

    // Profile is a single-row lookup; thread count walks up to
    // THREAD_COUNT_LIMIT rows over HTTP/2. Running them in parallel
    // overlaps the larger stream with the smaller one — tonic
    // multiplexes streams on a shared channel. signal_count is a third
    // independent query (a scalar CountByCondition) that joins the same way.
    let (profile_res, count_res, signal_res) = tokio::join!(
        fetch_profile(channel.clone(), user_id),
        fetch_thread_count(channel.clone(), user_id),
        fetch_signal_count(channel, user_id),
    );
    let profile = profile_res?;
    let (thread_count, thread_count_truncated) = count_res?;
    let signal_count = signal_res?;
    Ok(PersonalityResponse {
        profile,
        thread_count,
        thread_count_truncated,
        signal_count,
    })
}

/// List the layer-1 personality signals that back the merged
/// profile. One `FindListByCondition` with an `external_id_prefix` plus a
/// `personality_signal` thread_filter pulls every signal in a single RPC —
/// the same shape the merge workflow and `list_summaries` use, avoiding the
/// per-thread N+1 a label-list-then-fetch would incur.
#[tauri::command]
pub async fn list_personality_signals(
    state: State<'_, AppState>,
    req: ListPersonalitySignalsRequest,
) -> AppResult<Vec<PersonalitySignal>> {
    let channel = state.memories_channel().await?;
    fetch_signals(channel, req.user_id.unwrap_or(1)).await
}

/// Fetch every usable layer-1 signal for `user_id` in a single
/// `FindListByCondition` (external_id_prefix + `personality_signal`
/// thread_filter — the merge workflow / `list_summaries` shape). Drops
/// no_signal rows via `memory_to_signal_entry`, so this is the single source
/// of truth for both the drawer and the badge count: a thread that was once
/// signal-bearing keeps its `personality_signal` label after a re-run flips
/// it to no_signal, and `CountByCondition` has no metadata filter to exclude
/// those — counting the filtered rows here is the only way the badge agrees
/// with the drawer and the merged profile.
async fn fetch_signals(
    channel: tonic::transport::Channel,
    user_id: i64,
) -> AppResult<Vec<PersonalitySignal>> {
    let mut client = MemoryServiceClient::new(channel);
    let request = mem_svc::FindMemoryListRequest {
        limit: Some(SIGNAL_THREAD_LIMIT),
        offset: None,
        roles: vec![mem_data::MessageRole::RoleAssistant as i32],
        user_id: Some(mem_data::UserId {
            value: PERSONALITY_USER_ID,
        }),
        thread_id: None,
        updated_after: None,
        updated_before: None,
        external_id: None,
        content_types: vec![mem_data::ContentType::Text as i32],
        thread_filter: Some(signal_thread_filter(user_id)),
        created_after: None,
        created_before: None,
        // UPDATED_DESC default: newest first so a fresh re-extraction
        // surfaces at the top while a generation run is still in progress.
        sort: None,
        external_id_prefix: Some(SIGNAL_EXTERNAL_ID_PREFIX.to_string()),
    };
    let mut stream = client.find_list_by_condition(request).await?.into_inner();
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        if let Some(signal) = memory_to_signal_entry(item?) {
            out.push(signal);
        }
    }
    Ok(out)
}

async fn fetch_profile(
    channel: tonic::transport::Channel,
    user_id: i64,
) -> AppResult<Option<PersonalityProfile>> {
    let mut client = MemoryServiceClient::new(channel);
    let target_external_id = profile_external_id(user_id);
    let request = mem_svc::FindMemoryListRequest {
        limit: Some(1),
        offset: None,
        roles: vec![],
        user_id: Some(mem_data::UserId {
            value: PERSONALITY_USER_ID,
        }),
        thread_id: None,
        updated_after: None,
        updated_before: None,
        external_id: Some(target_external_id.clone()),
        content_types: vec![],
        thread_filter: None,
        created_after: None,
        created_before: None,
        sort: None,
        external_id_prefix: None,
    };
    let mut stream = client.find_list_by_condition(request).await?.into_inner();
    while let Some(item) = stream.next().await {
        if let Some(profile) = entry_to_profile(item?) {
            return Ok(Some(profile));
        }
    }
    Ok(None)
}

async fn fetch_thread_count(
    channel: tonic::transport::Channel,
    user_id: i64,
) -> AppResult<(i64, bool)> {
    let mut client = ThreadServiceClient::new(channel);
    // Request `limit + 1` so we can distinguish "exactly THREAD_COUNT_LIMIT
    // threads exist" (returns THREAD_COUNT_LIMIT rows -> not truncated)
    // from "more than THREAD_COUNT_LIMIT threads exist" (returns
    // THREAD_COUNT_LIMIT+1 rows -> truncated). The prior
    // `count == THREAD_COUNT_LIMIT` check reported truncated=true even
    // when the count was exact, so the UI rendered "10000+" for a user
    // with exactly 10_000 threads.
    let request = mem_svc::FindThreadListByUserIdRequest {
        user_id: Some(mem_data::UserId { value: user_id }),
        limit: Some(THREAD_COUNT_LIMIT + 1),
        offset: None,
        created_after: None,
        created_before: None,
        updated_after: None,
        updated_before: None,
        sort: None,
    };
    let mut stream = client
        .find_thread_list_by_user_id(request)
        .await?
        .into_inner();
    let mut count: i64 = 0;
    while let Some(item) = stream.next().await {
        // We don't need the payload, just to count successful rows.
        let _ = item?;
        count += 1;
    }
    let truncated = count > THREAD_COUNT_LIMIT as i64;
    // Clamp to the documented cap so the UI never renders a value above
    // the sentinel.
    let count = count.min(THREAD_COUNT_LIMIT as i64);
    Ok((count, truncated))
}

fn entry_to_profile(e: mem_svc::MemoryListEntry) -> Option<PersonalityProfile> {
    let memory = e.memory?;
    let memory_id = memory.id?.value;
    let data = memory.data?;
    Some(PersonalityProfile {
        memory_id,
        content_json: data.content,
        updated_at_ms: data.updated_at,
        external_id: data.external_id.unwrap_or_default(),
    })
}

fn profile_external_id(user_id: i64) -> String {
    format!("{PROFILE_EXTERNAL_ID_PREFIX}{user_id}")
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeletePersonalityMemoryRequest {
    #[serde(with = "crate::serde_id")]
    pub memory_id: i64,
}

/// Delete a single layer-1 personality signal. Signals are Memory rows owned by
/// `PERSONALITY_USER_ID`, so deletion is `MemoryService.Delete` by the backing
/// `memory_id` (surfaced on `PersonalitySignal`). The UI gates this behind a
/// confirm dialog.
#[tauri::command]
pub async fn delete_personality_signal(
    state: State<'_, AppState>,
    req: DeletePersonalityMemoryRequest,
) -> AppResult<()> {
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);
    client
        .delete(mem_data::MemoryId {
            value: req.memory_id,
        })
        .await?;
    Ok(())
}

/// Temporary investigation report for the "Signals count never grows" symptom.
/// Crosses three independent gRPC views over the `personality_user_id=200000`
/// space so we can tell apart the failure modes the merge skip / drawer empty
/// behaviour does not distinguish:
///
/// - `personality_user_threads_total`: every thread the personality user owns
///   (sanity check that thread-personality-single is creating its memory threads
///   at all)
/// - `personality_label_threads`: threads matching LABEL_ALL[personality,
///   user:<X>] — should equal `_total` minus the merged-profile thread
/// - `signal_label_threads`: threads also carrying `personality_signal` —
///   `applySignalLabel` is the only writer, so this gauges whether AddLabels
///   ever succeeds
/// - `signal_memories_total`: `MemoryService.FindListByCondition` with
///   `external_id_prefix=personality:` and no thread_filter — counts every
///   layer-1 memory regardless of label state
/// - `signal_memories_*`: the same population split by `metadata.no_signal`
///   so we can tell "LLM always returns no_signal=true" from "labels missing"
/// - `sample_signal_payload`: one ROLE_ASSISTANT memory's `content` (≤ 2000 chars)
///   so we can eyeball whether the LLM actually emitted interests / preferences
///   or just `{ "no_signal": true, "reason": "..." }`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PersonalityInventoryReport {
    pub personality_user_threads_total: i64,
    pub personality_label_threads: i64,
    pub signal_label_threads: i64,
    pub signal_memories_total: i64,
    pub signal_memories_with_metadata: i64,
    pub signal_memories_no_signal_true: i64,
    pub signal_memories_no_signal_false: i64,
    pub signal_memories_no_signal_missing: i64,
    pub sample_signal_payload: Option<String>,
    pub sample_signal_metadata: Option<String>,
    /// A second sample slot specifically for a no_signal=true row, so the
    /// triage panel can show both shapes when the store contains a mix.
    /// `sample_signal_payload` still falls back to a no_signal row when
    /// no valid signal exists, so the UI always has something headline.
    pub sample_no_signal_payload: Option<String>,
    pub sample_no_signal_metadata: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DebugPersonalityInventoryRequest {
    pub user_id: Option<i64>,
}

#[tauri::command]
pub async fn debug_personality_inventory(
    state: State<'_, AppState>,
    req: DebugPersonalityInventoryRequest,
) -> AppResult<PersonalityInventoryReport> {
    let user_id = req.user_id.unwrap_or(1);
    let channel = state.memories_channel().await?;

    // (a) threads owned by personality_user — proves single-workflow create path
    let personality_user_threads_total = {
        let mut client = ThreadServiceClient::new(channel.clone());
        let mut stream = client
            .find_thread_list_by_user_id(mem_svc::FindThreadListByUserIdRequest {
                user_id: Some(mem_data::UserId {
                    value: PERSONALITY_USER_ID,
                }),
                limit: Some(50_000),
                offset: None,
                created_after: None,
                created_before: None,
                updated_after: None,
                updated_before: None,
                sort: None,
            })
            .await?
            .into_inner();
        let mut n: i64 = 0;
        while let Some(item) = stream.next().await {
            let _ = item?;
            n += 1;
        }
        n
    };

    // (b) LABEL_ALL[personality, user:<X>] — the merge population pre-filter
    let personality_label_threads = count_threads_by_labels(
        channel.clone(),
        &["personality", &format!("user:{user_id}")],
    )
    .await?;

    // (c) LABEL_ALL[personality, user:<X>, personality_signal] — what the merge
    // workflow / list_personality_signals actually consume.
    let signal_label_threads = count_threads_by_labels(
        channel.clone(),
        &[
            "personality",
            &format!("user:{user_id}"),
            SIGNAL_THREAD_LABEL,
        ],
    )
    .await?;

    // (d/e) all signal memories regardless of label state — exposes the
    // metadata.no_signal distribution. Critically, we do NOT filter by the
    // signal-thread label here: that lets us tell "LLM returns no_signal=true
    // every time" (rows exist, no_signal=true everywhere) from "AddLabels
    // silently fails" (rows exist with no_signal=false but no
    // personality_signal-tagged thread).
    let signal_memories = {
        let mut client = MemoryServiceClient::new(channel.clone());
        let mut stream = client
            .find_list_by_condition(mem_svc::FindMemoryListRequest {
                limit: Some(50_000),
                offset: None,
                roles: vec![mem_data::MessageRole::RoleAssistant as i32],
                user_id: Some(mem_data::UserId {
                    value: PERSONALITY_USER_ID,
                }),
                thread_id: None,
                updated_after: None,
                updated_before: None,
                external_id: None,
                content_types: vec![mem_data::ContentType::Text as i32],
                thread_filter: None,
                created_after: None,
                created_before: None,
                sort: None,
                external_id_prefix: Some(SIGNAL_EXTERNAL_ID_PREFIX.to_string()),
            })
            .await?
            .into_inner();
        let mut rows: Vec<mem_data::Memory> = Vec::new();
        while let Some(item) = stream.next().await {
            if let Some(m) = item?.memory {
                rows.push(m);
            }
        }
        rows
    };

    let mut total: i64 = 0;
    let mut with_metadata: i64 = 0;
    let mut no_signal_true: i64 = 0;
    let mut no_signal_false: i64 = 0;
    let mut no_signal_missing: i64 = 0;
    // Capture TWO independent samples — one valid signal (no_signal=false)
    // and one no_signal row — so the triage panel can show "the LLM does
    // produce real signals" AND "this is what the no_signal reason looks
    // like" side by side. The prior single-sample design latched on the
    // first row matching `is_signal || total<=3`, which on a store like
    // 486/513 no_signal=true (the user's reported state) always captured a
    // no_signal=true payload and never showed a valid one even when 27
    // existed. Keeping payload+metadata in a tuple eliminates the latch
    // skew where `sample_metadata` was left None forever if the first
    // captured row had empty metadata while later rows had it.
    let mut sample_signal: Option<(String, Option<String>)> = None;
    let mut sample_no_signal: Option<(String, Option<String>)> = None;
    for memory in &signal_memories {
        total += 1;
        let Some(data) = &memory.data else { continue };
        let meta_str = data.metadata.as_deref();
        if meta_str.is_some_and(|s| !s.is_empty()) {
            with_metadata += 1;
        }
        let parsed: Option<serde_json::Value> = meta_str
            .filter(|s| !s.is_empty())
            .and_then(|s| serde_json::from_str(s).ok());
        let no_signal_flag = parsed.as_ref().and_then(|m| m.get("no_signal"));
        match no_signal_flag {
            Some(serde_json::Value::Bool(true)) => no_signal_true += 1,
            Some(serde_json::Value::Bool(false)) => no_signal_false += 1,
            _ => no_signal_missing += 1,
        }
        let captured = (
            truncate_for_report(&data.content, 2000),
            meta_str.map(|s| truncate_for_report(s, 500)),
        );
        match no_signal_flag {
            Some(serde_json::Value::Bool(false)) if sample_signal.is_none() => {
                sample_signal = Some(captured);
            }
            Some(serde_json::Value::Bool(true)) if sample_no_signal.is_none() => {
                sample_no_signal = Some(captured);
            }
            _ => {}
        }
    }

    // Fallback: if we only have no_signal rows (or only valid), surface
    // whatever we got in the legacy `sample_signal_payload` slot so the UI
    // always has something to show on the headline area. The dedicated
    // no_signal slot stays separate when both exist.
    let (sample_signal_payload, sample_signal_metadata) = sample_signal
        .clone()
        .or_else(|| sample_no_signal.clone())
        .map(|(p, m)| (Some(p), m))
        .unwrap_or((None, None));
    let (sample_no_signal_payload, sample_no_signal_metadata) = sample_no_signal
        .map(|(p, m)| (Some(p), m))
        .unwrap_or((None, None));

    Ok(PersonalityInventoryReport {
        personality_user_threads_total,
        personality_label_threads,
        signal_label_threads,
        signal_memories_total: total,
        signal_memories_with_metadata: with_metadata,
        signal_memories_no_signal_true: no_signal_true,
        signal_memories_no_signal_false: no_signal_false,
        signal_memories_no_signal_missing: no_signal_missing,
        sample_signal_payload,
        sample_signal_metadata,
        sample_no_signal_payload,
        sample_no_signal_metadata,
    })
}

async fn count_threads_by_labels(
    channel: tonic::transport::Channel,
    labels: &[&str],
) -> AppResult<i64> {
    let mut client = ThreadServiceClient::new(channel);
    let mut stream = client
        .find_thread_list_by_labels(mem_svc::FindThreadListByLabelsRequest {
            labels: labels.iter().map(|s| (*s).to_string()).collect(),
            match_mode: Some(mem_data::LabelMatchMode::LabelAll as i32),
            limit: Some(50_000),
            offset: None,
            user_id: Some(PERSONALITY_USER_ID),
            created_after: None,
            created_before: None,
            updated_after: None,
            updated_before: None,
            sort: None,
        })
        .await?
        .into_inner();
    let mut n: i64 = 0;
    while let Some(item) = stream.next().await {
        let _ = item?;
        n += 1;
    }
    Ok(n)
}

fn truncate_for_report(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…(truncated, total {} chars)", s.chars().count())
    }
}

/// Delete the merged personality profile (also a Memory row owned by
/// `PERSONALITY_USER_ID`). Same `MemoryService.Delete` path as a signal; kept
/// as a separate command so the call site documents intent and leaves room for
/// profile-specific behavior to diverge later.
#[tauri::command]
pub async fn delete_personality_profile(
    state: State<'_, AppState>,
    req: DeletePersonalityMemoryRequest,
) -> AppResult<()> {
    let mut client = MemoryServiceClient::new(state.memories_channel().await?);
    client
        .delete(mem_data::MemoryId {
            value: req.memory_id,
        })
        .await?;
    Ok(())
}

/// Thread-side filter narrowing to this user's `personality_signal`-tagged
/// threads. Mirrors user-personality-merge.yaml's `findSignalThreads` /
/// `countTotalPersonalityMemories` filter so badge, drawer, and merge all
/// see the same population.
fn signal_thread_filter(user_id: i64) -> mem_data::ThreadSearchFilter {
    mem_data::ThreadSearchFilter {
        // Signals are owned by the personality user, not the source user.
        user_id: Some(PERSONALITY_USER_ID),
        labels: vec![
            PERSONALITY_THREAD_LABEL.to_string(),
            format!("user:{user_id}"),
            SIGNAL_THREAD_LABEL.to_string(),
        ],
        label_match_mode: Some(mem_data::LabelMatchMode::LabelAll as i32),
        channel: None,
        created_after: None,
        created_before: None,
        updated_after: None,
        updated_before: None,
    }
}

/// Badge "Signals" count. Must equal the drawer's row count, so it must apply
/// the same `metadata.no_signal == false` filter the drawer does — a
/// `CountByCondition` has no metadata filter and would over-count threads
/// whose `personality_signal` label is sticky after a no_signal=true flip.
///
/// The prior implementation called `fetch_signals` and returned `.len()`,
/// which materialised up to `SIGNAL_THREAD_LIMIT` (10_000) full
/// `PersonalitySignal` payloads — clone of `content` + metadata strings per
/// row — on every Personality tab open. This variant streams the same RPC
/// but inspects only the (small) metadata JSON for `no_signal`, dropping the
/// `content` clone and the `PersonalitySignal` allocation; the result is
/// numerically identical to `fetch_signals().len()` but the per-row cost is
/// O(len(metadata)) instead of O(len(content)+len(metadata)).
async fn fetch_signal_count(channel: tonic::transport::Channel, user_id: i64) -> AppResult<i64> {
    let mut client = MemoryServiceClient::new(channel);
    let request = mem_svc::FindMemoryListRequest {
        limit: Some(SIGNAL_THREAD_LIMIT),
        offset: None,
        roles: vec![mem_data::MessageRole::RoleAssistant as i32],
        user_id: Some(mem_data::UserId {
            value: PERSONALITY_USER_ID,
        }),
        thread_id: None,
        updated_after: None,
        updated_before: None,
        external_id: None,
        content_types: vec![mem_data::ContentType::Text as i32],
        thread_filter: Some(signal_thread_filter(user_id)),
        created_after: None,
        created_before: None,
        sort: None,
        external_id_prefix: Some(SIGNAL_EXTERNAL_ID_PREFIX.to_string()),
    };
    let mut stream = client.find_list_by_condition(request).await?.into_inner();
    let mut count: i64 = 0;
    while let Some(item) = stream.next().await {
        if metadata_indicates_signal(item?) {
            count += 1;
        }
    }
    Ok(count)
}

/// Returns true if the row should be counted as a valid signal. Mirrors
/// the no_signal classification in `memory_to_signal` (metadata first,
/// content fallback for legacy rows) but without cloning `data.content`
/// into the `PersonalitySignal` value object — only the no_signal-bearing
/// JSON is materialised, and only for the `metadata.no_signal` lookup.
fn metadata_indicates_signal(e: mem_svc::MemoryListEntry) -> bool {
    let Some(memory) = e.memory else { return false };
    let Some(data) = memory.data else {
        return false;
    };
    let external_id = data.external_id.as_deref().unwrap_or_default();
    if !external_id.starts_with(SIGNAL_EXTERNAL_ID_PREFIX) {
        return false;
    }
    let metadata: Option<serde_json::Value> = data
        .metadata
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(s).ok());
    let no_signal = metadata
        .as_ref()
        .and_then(|m| m.get("no_signal"))
        .and_then(serde_json::Value::as_bool)
        .or_else(|| {
            serde_json::from_str::<serde_json::Value>(&data.content)
                .ok()
                .and_then(|c| c.get("no_signal").and_then(serde_json::Value::as_bool))
        })
        .unwrap_or(false);
    !no_signal
}

fn memory_to_signal_entry(e: mem_svc::MemoryListEntry) -> Option<PersonalitySignal> {
    memory_to_signal(e.memory?)
}

/// Build a `PersonalitySignal` from a layer-1 memory, dropping no_signal
/// rows and non-personality memories. `source_thread_id` comes from
/// metadata, falling back to the `personality:<id>` external_id suffix.
fn memory_to_signal(m: mem_data::Memory) -> Option<PersonalitySignal> {
    let memory_id = m.id?.value;
    let data = m.data?;
    let external_id = data.external_id.as_deref().unwrap_or_default();
    if !external_id.starts_with(SIGNAL_EXTERNAL_ID_PREFIX) {
        return None;
    }

    let metadata: serde_json::Value = data
        .metadata
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);

    // metadata.no_signal wins; fall back to the content payload's flag so a
    // legacy row written before the metadata convention is still filtered.
    let no_signal = metadata
        .get("no_signal")
        .and_then(serde_json::Value::as_bool)
        .or_else(|| {
            serde_json::from_str::<serde_json::Value>(&data.content)
                .ok()
                .and_then(|c| c.get("no_signal").and_then(serde_json::Value::as_bool))
        })
        .unwrap_or(false);
    if no_signal {
        return None;
    }

    let source_thread_id = metadata
        .get("source_thread_id")
        .and_then(parse_id_value)
        .or_else(|| super::parse_i64_after_prefix(SIGNAL_EXTERNAL_ID_PREFIX, Some(external_id)))?;

    Some(PersonalitySignal {
        memory_id,
        source_thread_id,
        content_json: data.content,
        updated_at_ms: data.updated_at,
    })
}

/// Parse an id from JSON that may encode int64 as a string (proto JSON) or
/// a number. The workflow stringifies source_thread_id to dodge JS int64
/// rounding, but accept both shapes defensively.
fn parse_id_value(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::String(s) => s.parse::<i64>().ok(),
        serde_json::Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_personality_memory_request_deserializes_memory_id_from_json_string() {
        let req: DeletePersonalityMemoryRequest =
            serde_json::from_str(r#"{"memory_id":"9007199254740993"}"#).unwrap();
        assert_eq!(req.memory_id, 9_007_199_254_740_993);
    }

    #[test]
    fn profile_external_id_uses_documented_prefix() {
        // Workflow YAML emits this exact form (see
        // lang-workers/workers/personality/user-personality-merge.yaml); a typo
        // here would silently return None when the row is in fact present.
        assert_eq!(profile_external_id(1), "personality_profile:1");
        assert_eq!(profile_external_id(42), "personality_profile:42");
    }

    #[test]
    fn entry_to_profile_pulls_content_updated_at_and_external_id() {
        let entry = mem_svc::MemoryListEntry {
            memory: Some(mem_data::Memory {
                id: Some(mem_data::MemoryId { value: 1 }),
                data: Some(mem_data::MemoryData {
                    content: "{\"profile_version\": \"1.0\"}".into(),
                    updated_at: 42,
                    external_id: Some("personality_profile:1".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let profile = entry_to_profile(entry).expect("present");
        assert_eq!(profile.memory_id, 1);
        assert_eq!(profile.content_json, "{\"profile_version\": \"1.0\"}");
        assert_eq!(profile.updated_at_ms, 42);
        assert_eq!(profile.external_id, "personality_profile:1");
    }

    #[test]
    fn entry_to_profile_returns_none_when_data_missing() {
        let entry = mem_svc::MemoryListEntry {
            memory: Some(mem_data::Memory {
                id: Some(mem_data::MemoryId { value: 1 }),
                data: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(entry_to_profile(entry).is_none());
    }

    fn signal_memory(
        external_id: &str,
        content: &str,
        metadata: Option<&str>,
        updated_at: i64,
    ) -> mem_data::Memory {
        mem_data::Memory {
            id: Some(mem_data::MemoryId { value: 1 }),
            data: Some(mem_data::MemoryData {
                content: content.into(),
                updated_at,
                external_id: Some(external_id.into()),
                metadata: metadata.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn signal_thread_filter_matches_merge_workflow() {
        // Raw literals (not the label constants) so a typo in a constant is
        // caught: user-personality-merge.yaml findSignalThreads filters on
        // exactly these three labels + LABEL_ALL, scoped to the personality
        // user. Drift here would list a different population than the merge.
        let filter = signal_thread_filter(1);
        assert_eq!(
            filter.labels,
            vec![
                "personality".to_string(),
                "user:1".to_string(),
                "personality_signal".to_string()
            ]
        );
        assert_eq!(filter.user_id, Some(PERSONALITY_USER_ID));
        assert_eq!(
            filter.label_match_mode,
            Some(mem_data::LabelMatchMode::LabelAll as i32)
        );
    }

    #[test]
    fn memory_to_signal_uses_metadata_source_thread_id() {
        let m = signal_memory(
            "personality:999",
            "{\"interests\":[]}",
            Some("{\"source_thread_id\":\"12345\",\"no_signal\":false}"),
            42,
        );
        let signal = memory_to_signal(m).expect("present");
        assert_eq!(signal.memory_id, 1);
        assert_eq!(signal.source_thread_id, 12345);
        assert_eq!(signal.content_json, "{\"interests\":[]}");
        assert_eq!(signal.updated_at_ms, 42);
    }

    #[test]
    fn memory_to_signal_falls_back_to_external_id_suffix() {
        // No metadata: the source thread id must be recovered from the
        // `personality:<id>` external_id.
        let m = signal_memory("personality:777", "{\"interests\":[]}", None, 7);
        let signal = memory_to_signal(m).expect("present");
        assert_eq!(signal.source_thread_id, 777);
    }

    #[test]
    fn memory_to_signal_drops_no_signal_rows() {
        let from_metadata = signal_memory(
            "personality:1",
            "{\"no_signal\":false}",
            Some("{\"source_thread_id\":\"1\",\"no_signal\":true}"),
            1,
        );
        assert!(memory_to_signal(from_metadata).is_none());

        // Legacy row without metadata.no_signal: the content flag still filters.
        let from_content = signal_memory("personality:2", "{\"no_signal\":true}", None, 1);
        assert!(memory_to_signal(from_content).is_none());
    }

    #[test]
    fn memory_to_signal_ignores_non_personality_external_id() {
        let m = signal_memory("summary:1", "{}", None, 1);
        assert!(memory_to_signal(m).is_none());
    }
}
