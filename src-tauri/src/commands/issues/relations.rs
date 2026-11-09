use std::future::Future;

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::{
    error::{AppError, AppResult},
    grpc::{
        issue_tracker::ManagerWriteRequest,
        proto::issue_tracker::{manager::v1 as manager, v1},
    },
};

use super::super::AppState;
use super::common::{human_actor, map_client_error, parse_revision, require_rationale};
use super::query::*;
use super::review::{
    IssueListItem, await_mutation, explicit_issue_draft_from_fields, issue_provenance_source,
    issue_to_list_item,
};

#[derive(Debug, Clone, Serialize)]
pub struct IssueDuplicate {
    pub id: String,
    pub title: String,
    pub description: String,
    pub state: String,
    pub completion_criteria: Vec<String>,
    pub evidence_count: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateIssueFromCanceledRequest {
    pub source_issue_id: String,
    pub expected_source_revision: String,
    pub operation_id: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub priority: String,
    pub suggested_state: String,
    pub acceptance_criteria: Vec<String>,
    pub rationale: String,
    pub link_to_source: bool,
}
/// Creates a new explicit Issue using a canceled Issue only as an editable
/// starting point. The canceled Issue remains immutable audit history.
pub(super) async fn create_issue_from_canceled_inner(
    state: State<'_, AppState>,
    req: CreateIssueFromCanceledRequest,
) -> AppResult<IssueListItem> {
    let rationale = require_rationale(Some(req.rationale.trim()))?.to_owned();
    validate_recreation_operation_id(&req.operation_id)?;
    let service = state.issue_tracker_service().await?;
    let source = service
        .get_issue_with_sync(
            "canceled-issue-recreate-source",
            manager::GetIssueWithSyncArgs {
                issue_id: req.source_issue_id.clone(),
                ..Default::default()
            },
        )
        .await
        .map_err(map_client_error)?
        .issue
        .ok_or_else(|| AppError::IssueTrackerServer("Source Issue is missing".into()))?;
    if source.state != v1::IssueState::Canceled as i32
        || source.revision != parse_revision(&req.expected_source_revision)?
    {
        return Err(AppError::IssueTrackerServer(
            "Source Issue has changed; refresh before creating a new Issue".into(),
        ));
    }

    let issue_id = recreated_issue_id(&source.issue_id, &req.operation_id);
    let mutation = service
        .create_issue(ManagerWriteRequest {
            correlation_id: format!("recreate-canceled-{}", req.operation_id),
            idempotency_key: format!("recreate-canceled-create-{}", req.operation_id),
            actor_audit: Some(human_actor(Some(format!(
                "取消済み Issue {} から新規作成: {rationale}",
                source.issue_id
            )))),
            payload: manager::CreateIssueArgs {
                issue_id: issue_id.clone(),
                issue: Some(explicit_issue_draft_from_canceled(
                    &req,
                    &source.source_evidence,
                )?),
                provenance_sources: vec![issue_provenance_source(&source)],
                ..Default::default()
            },
        })
        .await
        .map_err(map_client_error)?;
    await_mutation(&service, "recreate-canceled-create-await", mutation).await?;

    let created = service
        .get_issue_with_sync(
            "recreate-canceled-created",
            manager::GetIssueWithSyncArgs {
                issue_id: issue_id.clone(),
                ..Default::default()
            },
        )
        .await
        .map_err(map_client_error)?
        .issue
        .ok_or_else(|| AppError::IssueTrackerServer("Created Issue is missing".into()))?;
    if req.link_to_source {
        let source = service
            .get_issue_with_sync(
                "recreate-canceled-link-source",
                manager::GetIssueWithSyncArgs {
                    issue_id: req.source_issue_id.clone(),
                    ..Default::default()
                },
            )
            .await
            .map_err(map_client_error)?
            .issue
            .ok_or_else(|| AppError::IssueTrackerServer("Source Issue is missing".into()))?;
        let relation_id = recreated_relation_id(&created.issue_id, &source.issue_id);
        if !issue_relation_exists(&service, &created.issue_id, &relation_id).await? {
            let mutation = service
                .add_issue_relation(ManagerWriteRequest {
                    correlation_id: format!("recreate-canceled-link-{}", req.operation_id),
                    idempotency_key: format!("recreate-canceled-link-{}", req.operation_id),
                    actor_audit: Some(human_actor(Some(rationale))),
                    payload: manager::AddIssueRelationArgs {
                        relation_id,
                        kind: v1::RelationKind::Related as i32,
                        source_issue_id: created.issue_id.clone(),
                        source_expected_revision: Some(created.revision),
                        target_issue_id: source.issue_id,
                        target_expected_revision: Some(source.revision),
                        ..Default::default()
                    },
                })
                .await
                .map_err(map_client_error)?;
            await_mutation(&service, "recreate-canceled-link-await", mutation).await?;
        }
    }
    let created = service
        .get_issue_with_sync(
            "recreate-canceled-result",
            manager::GetIssueWithSyncArgs {
                issue_id,
                ..Default::default()
            },
        )
        .await
        .map_err(map_client_error)?;
    issue_to_list_item(created)
        .ok_or_else(|| AppError::IssueTrackerServer("Created Issue is missing".into()))
}
pub(super) async fn get_issue_duplicates_inner(
    state: State<'_, AppState>,
    item_id: String,
) -> AppResult<Vec<IssueDuplicate>> {
    let service = state.issue_tracker_service().await?;
    let candidate = service
        .get_candidate(
            "issue-duplicates",
            manager::GetCandidateArgs {
                candidate_id: item_id,
                ..Default::default()
            },
        )
        .await
        .map_err(map_client_error)?;
    Ok(
        readable_duplicates(candidate.duplicate_candidate_ids, |duplicate_id| {
            service.get_candidate(
                "issue-duplicate",
                manager::GetCandidateArgs {
                    candidate_id: duplicate_id,
                    ..Default::default()
                },
            )
        })
        .await,
    )
}

/// Independent lookups run concurrently; unreadable duplicates are skipped
/// and the result keeps the candidate's duplicate order.
async fn readable_duplicates<E, F, Fut>(duplicate_ids: Vec<String>, fetch: F) -> Vec<IssueDuplicate>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<manager::Candidate, E>>,
{
    futures::future::join_all(duplicate_ids.into_iter().map(fetch))
        .await
        .into_iter()
        .flatten()
        .map(|duplicate| {
            let state = candidate_status(&duplicate).to_owned();
            let evidence_count = duplicate.source_evidence.len();
            IssueDuplicate {
                id: duplicate.candidate_id,
                title: duplicate.title,
                description: duplicate.description,
                state,
                completion_criteria: duplicate.acceptance_criteria,
                evidence_count,
            }
        })
        .collect()
}

fn candidate_status(candidate: &manager::Candidate) -> &'static str {
    match manager::CandidateStatus::try_from(candidate.status).ok() {
        Some(manager::CandidateStatus::Accepted)
        | Some(manager::CandidateStatus::AutoRegistered) => "OPEN",
        Some(manager::CandidateStatus::Rejected) => "CANCELED",
        Some(manager::CandidateStatus::Superseded) => "DEFERRED",
        _ => "OPEN",
    }
}

fn explicit_issue_draft_from_canceled(
    request: &CreateIssueFromCanceledRequest,
    source_evidence: &[v1::SourceEvidenceRef],
) -> AppResult<v1::IssueDraft> {
    explicit_issue_draft_from_fields(
        &request.title,
        &request.description,
        &request.kind,
        &request.priority,
        &request.suggested_state,
        &request.acceptance_criteria,
    )
    .map(|mut draft| {
        draft.source_evidence = source_evidence.to_vec();
        draft
    })
}
fn recreated_issue_id(source_issue_id: &str, operation_id: &str) -> String {
    format!("recreated-from-{source_issue_id}-{operation_id}")
}

fn recreated_relation_id(new_issue_id: &str, source_issue_id: &str) -> String {
    format!("related-{new_issue_id}-{source_issue_id}")
}

fn validate_recreation_operation_id(value: &str) -> AppResult<()> {
    if !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Ok(());
    }
    Err(AppError::Config(
        "Creation operation ID must contain only ASCII letters, digits, or hyphens".into(),
    ))
}
async fn issue_relation_exists(
    service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
    issue_id: &str,
    relation_id: &str,
) -> AppResult<bool> {
    let issue_id = issue_id.to_owned();
    issue_relation_exists_in_pages(
        |page_token| async {
            let page = service
                .get_issue_relations(
                    "stale-replacement-relation-read",
                    manager::GetIssueRelationsArgs {
                        issue_id: issue_id.clone(),
                        page: Some(manager::PageRequest {
                            page_size: 100,
                            page_token,
                        }),
                        ..Default::default()
                    },
                )
                .await
                .map_err(map_client_error)?;
            Ok((
                page.relations,
                page.page.and_then(|page| page.next_page_token),
            ))
        },
        relation_id,
    )
    .await
}

async fn issue_relation_exists_in_pages<F, Fut>(fetch: F, relation_id: &str) -> AppResult<bool>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = AppResult<(Vec<v1::IssueRelation>, Option<String>)>>,
{
    let mut found = false;
    for_each_page(fetch, |relations| {
        found = relations
            .iter()
            .any(|relation| relation.relation_id == relation_id);
        !found
    })
    .await?;
    Ok(found)
}
#[cfg(test)]
mod issue_relation_tests {
    use super::*;

    #[tokio::test]
    async fn duplicate_lookups_run_concurrently_skip_unreadable_ones_and_keep_order() {
        let ids: Vec<String> = ["a", "b", "c", "d"].map(str::to_owned).to_vec();
        let gate = crate::commands::concurrency_probe::ConcurrencyGate::new(ids.len());
        let duplicates = readable_duplicates(ids.clone(), |id| {
            let gate = &gate;
            async move {
                let result = if id == "c" {
                    Err("unreadable")
                } else {
                    Ok(manager::Candidate {
                        title: format!("title-{id}"),
                        candidate_id: id,
                        ..Default::default()
                    })
                };
                gate.call(result).await
            }
        })
        .await;

        assert_eq!(
            duplicates.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "d"]
        );
        assert_eq!(duplicates[0].title, "title-a");
    }

    #[tokio::test]
    async fn relation_scan_finds_a_match_on_a_later_page() {
        let result = issue_relation_exists_in_pages(
            |page_token| async move {
                if page_token.is_none() {
                    Ok((vec![v1::IssueRelation::default()], Some("next-page".into())))
                } else {
                    Ok((
                        vec![v1::IssueRelation {
                            relation_id: "target-relation".into(),
                            ..Default::default()
                        }],
                        None,
                    ))
                }
            },
            "target-relation",
        )
        .await
        .expect("relation scan succeeds");

        assert!(result);
    }

    #[tokio::test]
    async fn relation_scan_rejects_pages_over_the_safe_client_limit() {
        let result = issue_relation_exists_in_pages(
            |page_token| async move {
                if page_token.is_none() {
                    Ok((
                        vec![v1::IssueRelation::default(); MAX_LIST_ITEMS],
                        Some("next-page".into()),
                    ))
                } else {
                    Ok((
                        vec![v1::IssueRelation::default()],
                        Some("still-next".into()),
                    ))
                }
            },
            "missing-relation",
        )
        .await;

        assert!(matches!(
            result,
            Err(AppError::IssueTrackerServer(message))
                if message == "Issue Tracker list exceeded the safe client limit"
        ));
    }
}
