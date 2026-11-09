//! Source-reference resolution for Issue review and acceptance.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use tokio_stream::StreamExt;

use crate::{
    error::AppResult,
    grpc::proto::{
        issue_tracker::v1,
        llm_memory::{
            data as mem_data, service as mem_svc,
            service::thread_service_client::ThreadServiceClient,
        },
    },
};

use super::super::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct IssueEvidence {
    pub thread_id: String,
    pub memory_id: String,
    pub excerpt: Option<String>,
    pub availability: String,
    pub unavailable_reason: Option<String>,
}

pub(super) async fn resolve_source_evidence(
    state: &AppState,
    references: &[v1::SourceEvidenceRef],
) -> AppResult<Vec<IssueEvidence>> {
    let targets = source_evidence_targets(references);
    let mut excerpts_by_thread = HashMap::with_capacity(targets.len());
    let mut unavailable_threads = HashSet::new();
    for (thread, memory_ids) in targets {
        let mut client = ThreadServiceClient::new(state.memories_channel().await?);
        let stream = client
            .find_memories_by_thread_id(mem_svc::FindMemoriesByThreadIdRequest {
                thread_id: Some(mem_data::ThreadId { value: thread }),
                limit: None,
                offset: None,
                roles: Vec::new(),
                content_types: Vec::new(),
            })
            .await;
        let Ok(response) = stream else {
            unavailable_threads.insert(thread);
            continue;
        };
        let mut stream = response.into_inner();
        let mut excerpts = HashMap::new();
        while let Some(row) = stream.next().await {
            match row {
                Ok(row) => {
                    if let Some(memory) = row.id.as_ref().map(|id| id.value)
                        && memory_ids.contains(&memory)
                        && let Some(data) = row.data
                    {
                        excerpts.insert(memory, data.content.chars().take(400).collect());
                    }
                }
                Err(_) => break,
            }
        }
        excerpts_by_thread.insert(thread, excerpts);
    }

    let mut resolved = Vec::with_capacity(references.len());
    for reference in references {
        let thread_id = reference.thread_id.trim().to_owned();
        let memory_id = reference.memory_id.trim().to_owned();
        let (Ok(thread), Ok(memory)) = (thread_id.parse::<i64>(), memory_id.parse::<i64>()) else {
            resolved.push(IssueEvidence {
                thread_id,
                memory_id,
                excerpt: None,
                availability: "unavailable".into(),
                unavailable_reason: Some("原文参照の ID が無効です".into()),
            });
            continue;
        };
        let excerpt = excerpts_by_thread
            .get(&thread)
            .and_then(|excerpts| excerpts.get(&memory))
            .cloned();
        let available = excerpt.is_some();
        resolved.push(IssueEvidence {
            thread_id,
            memory_id,
            excerpt,
            availability: if available {
                "available"
            } else {
                "unavailable"
            }
            .into(),
            unavailable_reason: (!available).then(|| {
                if unavailable_threads.contains(&thread) {
                    "原文 Thread を取得できません".into()
                } else {
                    "原文 Memory を取得できません".into()
                }
            }),
        });
    }
    Ok(resolved)
}

pub(crate) async fn all_source_evidence_available(
    state: &AppState,
    references: &[v1::SourceEvidenceRef],
) -> bool {
    !references.is_empty()
        && resolve_source_evidence(state, references)
            .await
            .is_ok_and(|rows| rows.iter().all(|row| row.availability == "available"))
}

fn source_evidence_targets(references: &[v1::SourceEvidenceRef]) -> HashMap<i64, HashSet<i64>> {
    let mut targets = HashMap::new();
    for reference in references {
        let (Ok(thread_id), Ok(memory_id)) = (
            reference.thread_id.trim().parse::<i64>(),
            reference.memory_id.trim().parse::<i64>(),
        ) else {
            continue;
        };
        targets
            .entry(thread_id)
            .or_insert_with(HashSet::new)
            .insert(memory_id);
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_memory_reads_by_thread() {
        let references = vec![
            v1::SourceEvidenceRef {
                thread_id: "1".into(),
                memory_id: "10".into(),
            },
            v1::SourceEvidenceRef {
                thread_id: "1".into(),
                memory_id: "11".into(),
            },
            v1::SourceEvidenceRef {
                thread_id: "2".into(),
                memory_id: "20".into(),
            },
            v1::SourceEvidenceRef {
                thread_id: "invalid".into(),
                memory_id: "30".into(),
            },
        ];

        let targets = source_evidence_targets(&references);

        assert_eq!(targets.len(), 2);
        assert_eq!(targets.get(&1), Some(&HashSet::from([10, 11])));
        assert_eq!(targets.get(&2), Some(&HashSet::from([20])));
    }
}
