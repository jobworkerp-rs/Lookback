use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::connection::ConnectionMode;
use super::guards::ImportSource;
use super::request::ImportStep;
use crate::error::{AppError, AppResult};
use crate::grpc::proto::llm_memory::data as mem_data;
use crate::grpc::proto::llm_memory::service as mem_svc;
use crate::grpc::proto::llm_memory::service::thread_service_client::ThreadServiceClient;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct PendingImportSession {
    pub(super) memories_endpoint_id: String,
    pub(super) user_id: i64,
    pub(super) source: ImportSource,
    pub(super) session_key: String,
    pub(super) thread_id: String,
    #[serde(default)]
    pub(super) lifecycle_id: Option<String>,
    #[serde(default)]
    pub(super) plan: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) steps: BTreeMap<String, PendingImportStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct PendingImportStep {
    pub(super) operation_key: String,
    #[serde(default)]
    pub(super) job_id: Option<String>,
    #[serde(default)]
    pub(super) succeeded: bool,
    #[serde(default)]
    pub(super) retained: bool,
}

pub(super) const PENDING_IMPORT_FORMAT_VERSION: u32 = 3;

impl PendingImportSession {
    /// Canonical identity of a pending session. `thread_id` is a numeric
    /// handed out by whichever Memories server produced it, so two sessions
    /// in the single global `import-pending.json` can share one while
    /// belonging to different endpoints, users or sources. Every lookup that
    /// mutates a stored session keys on this tuple instead.
    pub(super) fn identity(&self) -> (&str, i64, &'static str, &str) {
        (
            self.memories_endpoint_id.as_str(),
            self.user_id,
            self.source.cli_subcommand(),
            self.session_key.as_str(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingImportScope {
    pub(super) memories_endpoint_id: String,
    pub(super) user_id: i64,
}

impl PendingImportScope {
    pub(super) fn new(mode: ConnectionMode, memories_url: &str, user_id: i64) -> AppResult<Self> {
        Ok(Self {
            memories_endpoint_id: pending_memories_endpoint_id(mode, memories_url)?,
            user_id,
        })
    }

    pub(super) fn contains(&self, session: &PendingImportSession) -> bool {
        session.memories_endpoint_id == self.memories_endpoint_id && session.user_id == self.user_id
    }
}

fn pending_memories_endpoint_id(mode: ConnectionMode, memories_url: &str) -> AppResult<String> {
    if mode == ConnectionMode::Local {
        return Ok("local".into());
    }
    let url = url::Url::parse(memories_url)
        .map_err(|error| AppError::Config(format!("invalid memories URL: {error}")))?;
    let scheme = match url.scheme() {
        "http" | "https" => url.scheme(),
        other => {
            return Err(AppError::Config(format!(
                "memories URL must be http or https, got {other}"
            )));
        }
    };
    let host = match url.host() {
        Some(url::Host::Domain(host)) => host.to_ascii_lowercase(),
        Some(url::Host::Ipv4(host)) => host.to_string(),
        Some(url::Host::Ipv6(host)) => format!("[{host}]"),
        None => return Err(AppError::Config("memories URL has no host".into())),
    };
    let port = url
        .port_or_known_default()
        .ok_or_else(|| AppError::Config("memories URL has no effective port".into()))?;
    let digest = Sha256::digest(format!("{scheme}://{host}:{port}").as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(format!("remote-sha256:{encoded}"))
}

pub(super) fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            let mut canonical = serde_json::Map::new();
            for key in keys {
                canonical.insert(key.clone(), canonical_json(&object[key]));
            }
            serde_json::Value::Object(canonical)
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(canonical_json).collect())
        }
        value => value.clone(),
    }
}

pub(super) fn generation_operation_key(
    scope: &PendingImportScope,
    sources: &[ImportSource],
    thread_ids: &[String],
    step: ImportStep,
    input: &serde_json::Value,
) -> String {
    let mut source_names = sources
        .iter()
        .map(|source| source.cli_subcommand())
        .collect::<Vec<_>>();
    source_names.sort_unstable();
    source_names.dedup();
    let mut thread_names = thread_ids.iter().map(String::as_str).collect::<Vec<_>>();
    thread_names.sort_unstable();
    thread_names.dedup();
    let input = serde_json::to_string(&canonical_json(input))
        .expect("serde_json::Value serialization cannot fail");
    let canonical = format!(
        "v1|endpoint={}|user={}|sources={}|threads={}|step={:?}|input={}",
        scope.memories_endpoint_id,
        scope.user_id,
        source_names.join(","),
        thread_names.join(","),
        step,
        input,
    );
    let digest = Sha256::digest(canonical.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("lookback-op-{encoded}")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct PendingImportState {
    pub(super) format_version: u32,
    pub(super) sessions: Vec<PendingImportSession>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct VerifiedImportThread {
    pub(super) thread_id: String,
    pub(super) user_id: i64,
    pub(super) memory_kind: i32,
    /// Conversation span (epoch ms), used to scope downstream group work.
    pub(super) first_message_at: Option<i64>,
    pub(super) last_message_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReconciledImportTargets {
    pub(super) target_thread_ids: Vec<String>,
    pub(super) candidate_intersection: Vec<String>,
    pub(super) supplemented_ids: Vec<String>,
}

pub(super) fn reconcile_import_targets(
    emitted_ids: &[String],
    candidate_ids: &[String],
    verified: &[VerifiedImportThread],
    expected_user_id: i64,
) -> AppResult<ReconciledImportTargets> {
    let candidates = candidate_ids.iter().collect::<BTreeSet<_>>();
    let verified_by_id = verified
        .iter()
        .map(|thread| (&thread.thread_id, thread))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut target_thread_ids = emitted_ids.to_vec();
    target_thread_ids.sort();
    target_thread_ids.dedup();
    let mut candidate_intersection = Vec::new();
    let mut supplemented_ids = Vec::new();
    for thread_id in &target_thread_ids {
        let thread = verified_by_id.get(thread_id).ok_or_else(|| {
            AppError::Config(format!("import target thread {thread_id} was not found"))
        })?;
        if thread.user_id != expected_user_id
            || thread.memory_kind != mem_data::MemoryKind::Raw as i32
        {
            return Err(AppError::Config(format!(
                "import target thread {thread_id} is not owned RAW data"
            )));
        }
        if candidates.contains(thread_id) {
            candidate_intersection.push(thread_id.clone());
        } else {
            supplemented_ids.push(thread_id.clone());
        }
    }
    Ok(ReconciledImportTargets {
        target_thread_ids,
        candidate_intersection,
        supplemented_ids,
    })
}

/// Exclusive epoch-ms window spanning the conversations of the import
/// targets, so the ThreadGroup step only visits groups those threads touch.
/// `None` when no target reports its conversation span.
pub(super) fn import_activity_window(
    verified: &[VerifiedImportThread],
    target_thread_ids: &[String],
) -> Option<(i64, i64)> {
    let targets = target_thread_ids.iter().collect::<BTreeSet<_>>();
    let (first, last) = verified
        .iter()
        .filter(|thread| targets.contains(&thread.thread_id))
        .filter_map(|thread| {
            Some((
                thread.first_message_at.or(thread.last_message_at)?,
                thread.last_message_at.or(thread.first_message_at)?,
            ))
        })
        .reduce(|(first, last), (start, end)| (first.min(start), last.max(end)))?;
    Some((first - 1, last + 1))
}

pub(super) async fn verify_import_threads(
    memories_url: &str,
    thread_ids: &[String],
) -> AppResult<Vec<VerifiedImportThread>> {
    let mut client = ThreadServiceClient::new(crate::grpc::connect(memories_url).await?);
    let mut verified = Vec::with_capacity(thread_ids.len());
    for thread_id in thread_ids {
        let numeric_id = thread_id.parse::<i64>().map_err(|_| {
            AppError::Config(format!("invalid pending import thread id {thread_id}"))
        })?;
        let response = client
            .find(mem_data::ThreadId { value: numeric_id })
            .await?
            .into_inner();
        let thread = response.data.ok_or_else(|| {
            AppError::Config(format!("import target thread {thread_id} is absent"))
        })?;
        let data = thread.data.ok_or_else(|| {
            AppError::Config(format!("import target thread {thread_id} has no data"))
        })?;
        verified.push(VerifiedImportThread {
            thread_id: thread_id.clone(),
            user_id: data.user_id.map(|id| id.value).unwrap_or_default(),
            memory_kind: data.memory_kind,
            first_message_at: data.first_message_at,
            last_message_at: data.last_message_at,
        });
    }
    Ok(verified)
}

pub(super) async fn find_import_window_candidates(
    memories_url: &str,
    user_id: i64,
    after: i64,
    before: i64,
) -> AppResult<Vec<String>> {
    if before <= after {
        return Ok(Vec::new());
    }
    let mut client = ThreadServiceClient::new(crate::grpc::connect(memories_url).await?);
    let request = mem_svc::FindThreadListByUserIdRequest {
        user_id: Some(mem_data::UserId { value: user_id }),
        limit: None,
        offset: None,
        sort: None,
        memory_kinds: vec![mem_data::MemoryKind::Raw as i32],
        thread_created_after: Some(after),
        thread_created_before: Some(before),
        thread_updated_after: None,
        thread_updated_before: None,
        first_message_after: None,
        first_message_before: None,
        last_message_after: None,
        last_message_before: None,
    };
    let mut stream = client
        .find_thread_list_by_user_id(request)
        .await?
        .into_inner();
    let mut ids = Vec::new();
    while let Some(thread) = tokio_stream::StreamExt::next(&mut stream).await {
        if let Some(id) = thread?.id {
            ids.push(id.value.to_string());
        }
    }
    Ok(ids)
}

impl Default for PendingImportState {
    fn default() -> Self {
        Self {
            format_version: PENDING_IMPORT_FORMAT_VERSION,
            sessions: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::guards::ImportSource;
    use super::*;
    use crate::grpc::proto::llm_memory::data as mem_data;

    #[test]
    fn generation_operation_key_is_canonical_and_step_scoped() {
        let scope = PendingImportScope {
            memories_endpoint_id: "local".into(),
            user_id: 1,
        };
        let first = generation_operation_key(
            &scope,
            &[ImportSource::OpenCode, ImportSource::Codex],
            &["20".into(), "10".into()],
            ImportStep::ThreadSummary,
            &serde_json::json!({"b": 2, "a": 1}),
        );
        let same_operation = generation_operation_key(
            &scope,
            &[ImportSource::Codex, ImportSource::OpenCode],
            &["10".into(), "20".into()],
            ImportStep::ThreadSummary,
            &serde_json::json!({"a": 1, "b": 2}),
        );
        let different_step = generation_operation_key(
            &scope,
            &[ImportSource::Codex, ImportSource::OpenCode],
            &["10".into(), "20".into()],
            ImportStep::ThreadPersonality,
            &serde_json::json!({"a": 1, "b": 2}),
        );

        assert_eq!(first, same_operation);
        assert_ne!(first, different_step);
        assert!(first.starts_with("lookback-op-"));
    }

    #[test]
    fn target_reconciliation_deduplicates_intersects_and_supplements() {
        let emitted = vec!["42".into(), "9007199254740993".into(), "42".into()];
        let candidates = vec!["42".into(), "999".into()];
        let verified = vec![
            VerifiedImportThread {
                thread_id: "42".into(),
                user_id: 1,
                memory_kind: mem_data::MemoryKind::Raw as i32,
                first_message_at: None,
                last_message_at: None,
            },
            VerifiedImportThread {
                thread_id: "9007199254740993".into(),
                user_id: 1,
                memory_kind: mem_data::MemoryKind::Raw as i32,
                first_message_at: None,
                last_message_at: None,
            },
        ];
        let result = reconcile_import_targets(&emitted, &candidates, &verified, 1).unwrap();
        assert_eq!(result.target_thread_ids, vec!["42", "9007199254740993"]);
        assert_eq!(result.candidate_intersection, vec!["42"]);
        assert_eq!(result.supplemented_ids, vec!["9007199254740993"]);
    }

    #[test]
    fn target_reconciliation_fails_closed_on_owner_kind_or_missing_id() {
        let emitted = vec!["42".into()];
        for verified in [
            Vec::new(),
            vec![VerifiedImportThread {
                thread_id: "42".into(),
                user_id: 2,
                memory_kind: mem_data::MemoryKind::Raw as i32,
                first_message_at: None,
                last_message_at: None,
            }],
            vec![VerifiedImportThread {
                thread_id: "42".into(),
                user_id: 1,
                memory_kind: mem_data::MemoryKind::ThreadSummary as i32,
                first_message_at: None,
                last_message_at: None,
            }],
        ] {
            assert!(reconcile_import_targets(&emitted, &[], &verified, 1).is_err());
        }
    }

    fn span_thread(id: &str, first: Option<i64>, last: Option<i64>) -> VerifiedImportThread {
        VerifiedImportThread {
            thread_id: id.into(),
            user_id: 1,
            memory_kind: mem_data::MemoryKind::Raw as i32,
            first_message_at: first,
            last_message_at: last,
        }
    }

    #[test]
    fn import_activity_window_spans_only_the_target_conversations() {
        let verified = vec![
            span_thread("1", Some(1_000), Some(1_500)),
            span_thread("2", Some(800), Some(2_000)),
            span_thread("3", Some(10), Some(99_999)),
        ];
        assert_eq!(
            import_activity_window(&verified, &["1".into(), "2".into()]),
            Some((799, 2_001))
        );
    }

    #[test]
    fn import_activity_window_uses_whichever_bound_a_thread_reports() {
        let verified = vec![
            span_thread("1", None, Some(500)),
            span_thread("2", Some(900), None),
        ];
        assert_eq!(
            import_activity_window(&verified, &["1".into(), "2".into()]),
            Some((499, 901))
        );
        assert_eq!(
            import_activity_window(&[span_thread("1", None, None)], &["1".into()]),
            None
        );
        assert_eq!(import_activity_window(&verified, &[]), None);
    }

    #[test]
    fn clock_reversal_does_not_drop_authoritative_emitted_ids() {
        let emitted = vec!["42".into()];
        let verified = vec![VerifiedImportThread {
            thread_id: "42".into(),
            user_id: 1,
            memory_kind: mem_data::MemoryKind::Raw as i32,
            first_message_at: None,
            last_message_at: None,
        }];
        let result = reconcile_import_targets(&emitted, &[], &verified, 1).unwrap();
        assert_eq!(result.supplemented_ids, vec!["42"]);
    }
}
