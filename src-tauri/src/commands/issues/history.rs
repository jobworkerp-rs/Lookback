use chrono::Utc;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::{
    error::{AppError, AppResult},
    grpc::{issue_tracker::IssueTrackerClientError, proto::issue_tracker::manager::v1 as manager},
};

use super::super::AppState;
use super::common::{durable_operation_state_name, map_client_error};
#[allow(unused_imports)]
pub(super) use super::query::for_each_page_capped;
use super::query::{
    list_issue_operation_history_page, list_operation_history_capped,
    manager_error_is_page_token_expired,
};

const MAX_HISTORY_ITEMS_PER_COLLECTION: usize = 500;
const MAX_HISTORY_TOTAL_ITEMS: usize = 1_000;
#[derive(Debug, Clone, Serialize)]
pub struct IssueHistoryEntry {
    pub id: String,
    pub created_at_ms: i64,
    pub actor: String,
    pub decision: String,
    pub target_title: String,
    pub issue_ids: Vec<String>,
    pub rationale: Option<String>,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueHistoryResponse {
    pub entries: Vec<IssueHistoryEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum IssueHistoryMode {
    Recovery,
    Audit,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListIssueDirectHistoryRequest {
    pub issue_id: String,
    pub mode: IssueHistoryMode,
    pub page_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueHistoryPage {
    pub entries: Vec<IssueHistoryEntry>,
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IssueHistoryPageOutcome {
    Page { page: IssueHistoryPage },
    ResetRequired,
}
/// Root operation methods shown in the Issues history view.
const ROOT_HISTORY_METHODS: [&str; 13] = [
    "manager_resolve_candidate",
    "manager_create_issue",
    "manager_update_issue",
    "manager_add_issue_relation",
    "manager_remove_issue_relation",
    "manager_add_issue_comment",
    "manager_claim_issue",
    "manager_heartbeat_issue_claim",
    "manager_release_issue_claim",
    "manager_transition_issue",
    "manager_reconcile_expired_claim",
    "manager_apply_issue_admin_action",
    "manager_resolve_issue_operation",
];

/// The per-method reads are independent, so they are issued concurrently
/// instead of paying one round trip per method; results keep method order.
async fn collect_root_history<T, F, Fut>(fetch: F) -> AppResult<(Vec<T>, bool)>
where
    F: Fn(&'static str) -> Fut,
    Fut: std::future::Future<Output = AppResult<(Vec<T>, bool)>>,
{
    let collections = futures::future::try_join_all(ROOT_HISTORY_METHODS.map(fetch)).await?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for (method_entries, method_truncated) in collections {
        entries.extend(method_entries);
        truncated |= method_truncated;
    }
    Ok((entries, truncated))
}

pub(super) async fn list_issue_history_inner(
    state: State<'_, AppState>,
) -> AppResult<IssueHistoryResponse> {
    let service = state.issue_tracker_service().await?;
    let window = service
        .root_operation_history_max_window_millis()
        .map_err(map_client_error)?;
    let now_ms = Utc::now().timestamp_millis();
    let (entries, truncated) = collect_root_history(|method| {
        list_operation_history_capped(
            &service,
            "issue-history",
            root_operation_history_filter(method, window, now_ms),
            MAX_HISTORY_ITEMS_PER_COLLECTION,
        )
    })
    .await?;
    Ok(finalize_issue_history_entries(entries, truncated))
}

pub(super) async fn list_issue_direct_history_inner(
    state: State<'_, AppState>,
    req: ListIssueDirectHistoryRequest,
) -> AppResult<IssueHistoryPageOutcome> {
    if req.issue_id.trim().is_empty() {
        return Err(AppError::Config("Issue ID is required".into()));
    }
    let issue_id = req.issue_id.trim().to_owned();
    let service = state.issue_tracker_service().await?;
    let filter = direct_history_filter(&issue_id, req.mode);
    let page_token_present = req
        .page_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty());
    match list_issue_operation_history_page(
        &service,
        "issue-direct-history",
        filter,
        req.page_token,
    )
    .await
    {
        Ok(page) => Ok(IssueHistoryPageOutcome::Page {
            // Keep the server-side direct-scope filter as the primary path, but
            // validate the direct target locally as well. Older compatible
            // servers may ignore the newly added `direct_scope_only` field and
            // otherwise return operations that merely affect this Issue.
            page: history_page_to_dto(page, &issue_id),
        }),
        Err(IssueTrackerClientError::Manager(error))
            if page_token_present && manager_error_is_page_token_expired(&error) =>
        {
            Ok(IssueHistoryPageOutcome::ResetRequired)
        }
        Err(error) => Err(map_client_error(error)),
    }
}

fn timestamp_ms(value: Option<&manager::UtcInstant>) -> i64 {
    value.map_or(0, |instant| instant.unix_epoch_millis)
}
fn history_to_dto(entry: manager::OperationHistoryEntry) -> IssueHistoryEntry {
    let target_title = match entry.direct_target.as_ref() {
        Some(manager::operation_history_entry::DirectTarget::CandidateId(id))
        | Some(manager::operation_history_entry::DirectTarget::EvidenceId(id))
        | Some(manager::operation_history_entry::DirectTarget::ExtractionRunId(id))
        | Some(manager::operation_history_entry::DirectTarget::IssueId(id)) => id.clone(),
        None => String::new(),
    };
    let mut issue_ids = entry.affected_issue_ids.clone();
    if let Some(manager::operation_history_entry::DirectTarget::IssueId(id)) =
        entry.direct_target.as_ref()
    {
        issue_ids.push(id.clone());
    }
    issue_ids.sort();
    issue_ids.dedup();
    IssueHistoryEntry {
        id: entry.operation_id,
        created_at_ms: timestamp_ms(entry.created_at.as_ref()),
        actor: entry
            .actor_audit
            .as_ref()
            .map(|actor| {
                actor
                    .display_name
                    .clone()
                    .unwrap_or_else(|| actor.actor_id.clone())
            })
            .unwrap_or_else(|| "system".into()),
        decision: entry
            .resolution_decision_kind
            .and_then(|value| manager::ResolutionDecisionKind::try_from(value).ok())
            .map_or_else(|| entry.method.clone(), |kind| format!("{kind:?}")),
        target_title,
        issue_ids,
        rationale: entry
            .resolution_rationale
            .or_else(|| entry.actor_audit.and_then(|actor| actor.reason)),
        state: manager::DurableOperationState::try_from(entry.state)
            .ok()
            .map(durable_operation_state_name)
            .unwrap_or_else(|| "UNKNOWN".into()),
    }
}

fn finalize_issue_history_entries(
    mut entries: Vec<manager::OperationHistoryEntry>,
    mut truncated: bool,
) -> IssueHistoryResponse {
    entries.sort_by_key(|entry| std::cmp::Reverse(timestamp_ms(entry.created_at.as_ref())));
    if entries.len() > MAX_HISTORY_TOTAL_ITEMS {
        entries.truncate(MAX_HISTORY_TOTAL_ITEMS);
        truncated = true;
    }
    IssueHistoryResponse {
        entries: entries.into_iter().map(history_to_dto).collect(),
        truncated,
    }
}

fn root_operation_history_filter(
    method: &str,
    max_window_millis: u64,
    now_millis: i64,
) -> manager::OperationHistoryFilter {
    let window_millis = i64::try_from(max_window_millis).unwrap_or(i64::MAX);
    manager::OperationHistoryFilter {
        method: Some(method.into()),
        created_at_from: Some(manager::UtcInstant {
            unix_epoch_millis: now_millis.saturating_sub(window_millis),
        }),
        created_at_to: Some(manager::UtcInstant {
            unix_epoch_millis: now_millis,
        }),
        ..Default::default()
    }
}

fn direct_history_filter(
    issue_id: &str,
    mode: IssueHistoryMode,
) -> manager::OperationHistoryFilter {
    let states = match mode {
        IssueHistoryMode::Recovery => vec![
            manager::DurableOperationState::Received as i32,
            manager::DurableOperationState::Pending as i32,
            manager::DurableOperationState::Failed as i32,
            manager::DurableOperationState::Unconfirmed as i32,
            manager::DurableOperationState::ManualResolution as i32,
        ],
        IssueHistoryMode::Audit => Vec::new(),
    };
    manager::OperationHistoryFilter {
        direct_target: Some(manager::operation_history_filter::DirectTarget::IssueId(
            issue_id.to_owned(),
        )),
        states,
        direct_scope_only: true,
        ..Default::default()
    }
}

fn history_page_to_dto(page: manager::OperationHistoryPage, issue_id: &str) -> IssueHistoryPage {
    IssueHistoryPage {
        entries: page
            .entries
            .into_iter()
            .filter(|entry| {
                matches!(
                    entry.direct_target.as_ref(),
                    Some(manager::operation_history_entry::DirectTarget::IssueId(target))
                        if target == issue_id
                )
            })
            .map(history_to_dto)
            .collect(),
        next_page_token: page
            .page
            .and_then(|page| page.next_page_token)
            .filter(|token| !token.is_empty()),
    }
}
#[cfg(test)]
mod issue_history_page_tests {
    use super::*;

    #[tokio::test]
    async fn root_history_reads_run_concurrently_and_keep_method_order() {
        let gate =
            crate::commands::concurrency_probe::ConcurrencyGate::new(ROOT_HISTORY_METHODS.len());
        let (entries, truncated) = collect_root_history(|method| {
            let gate = &gate;
            async move {
                Ok(gate
                    .call((vec![method.to_owned()], method == "manager_claim_issue"))
                    .await)
            }
        })
        .await
        .unwrap();

        assert_eq!(entries, ROOT_HISTORY_METHODS.map(str::to_owned).to_vec());
        assert!(
            truncated,
            "one truncated collection marks the history truncated"
        );
    }

    #[tokio::test]
    async fn root_history_reports_untruncated_and_propagates_errors() {
        let (_, truncated) = collect_root_history(|_| async { Ok((Vec::<String>::new(), false)) })
            .await
            .unwrap();
        assert!(!truncated);

        let err = collect_root_history(|method| async move {
            if method == "manager_transition_issue" {
                Err(AppError::Config("history read failed".into()))
            } else {
                Ok((Vec::<String>::new(), false))
            }
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Config(message) if message == "history read failed"));
    }

    fn operation_history_entry(id: &str, created_at_ms: i64) -> manager::OperationHistoryEntry {
        manager::OperationHistoryEntry {
            operation_id: id.into(),
            created_at: Some(manager::UtcInstant {
                unix_epoch_millis: created_at_ms,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn direct_history_filter_is_scoped_for_both_modes() {
        let recovery = direct_history_filter("issue-1", IssueHistoryMode::Recovery);
        assert_eq!(
            recovery.direct_target,
            Some(manager::operation_history_filter::DirectTarget::IssueId(
                "issue-1".into()
            ))
        );
        assert!(recovery.direct_scope_only);
        assert_eq!(
            recovery.states,
            vec![
                manager::DurableOperationState::Received as i32,
                manager::DurableOperationState::Pending as i32,
                manager::DurableOperationState::Failed as i32,
                manager::DurableOperationState::Unconfirmed as i32,
                manager::DurableOperationState::ManualResolution as i32,
            ]
        );

        let audit = direct_history_filter("issue-1", IssueHistoryMode::Audit);
        assert_eq!(audit.direct_target, recovery.direct_target);
        assert!(audit.direct_scope_only);
        assert!(audit.states.is_empty());
    }

    #[test]
    fn history_page_outcome_has_explicit_reset_discriminant() {
        let outcome = IssueHistoryPageOutcome::ResetRequired;
        let value = serde_json::to_value(outcome).expect("outcome serializes");
        assert_eq!(value["kind"], "reset_required");
    }

    #[test]
    fn history_page_keeps_the_server_cursor_opaque() {
        let page = manager::OperationHistoryPage {
            entries: vec![manager::OperationHistoryEntry {
                operation_id: "operation-1".into(),
                direct_target: Some(manager::operation_history_entry::DirectTarget::IssueId(
                    "issue-1".into(),
                )),
                ..Default::default()
            }],
            page: Some(manager::PageInfo {
                next_page_token: Some("operation-history-page-v2:opaque".into()),
                ..Default::default()
            }),
        };
        let mapped = history_page_to_dto(page, "issue-1");
        assert_eq!(mapped.entries[0].id, "operation-1");
        assert_eq!(
            mapped.next_page_token.as_deref(),
            Some("operation-history-page-v2:opaque")
        );
    }

    #[test]
    fn history_page_drops_operations_that_only_affect_the_issue() {
        let page = manager::OperationHistoryPage {
            entries: vec![
                manager::OperationHistoryEntry {
                    operation_id: "direct-operation".into(),
                    direct_target: Some(manager::operation_history_entry::DirectTarget::IssueId(
                        "issue-1".into(),
                    )),
                    ..Default::default()
                },
                manager::OperationHistoryEntry {
                    operation_id: "affected-only-operation".into(),
                    direct_target: Some(manager::operation_history_entry::DirectTarget::IssueId(
                        "issue-2".into(),
                    )),
                    affected_issue_ids: vec!["issue-1".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let mapped = history_page_to_dto(page, "issue-1");
        assert_eq!(
            mapped
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["direct-operation"]
        );
    }

    #[test]
    fn issue_history_under_the_limits_is_not_marked_truncated() {
        let response =
            finalize_issue_history_entries(vec![operation_history_entry("history-1", 1)], false);

        assert_eq!(response.entries.len(), 1);
        assert!(!response.truncated);
    }

    #[tokio::test]
    async fn history_collection_stops_at_its_cap_without_error() {
        let mut collected = Vec::new();
        let mut fetch_count = 0;
        let truncated = for_each_page_capped(
            |page_token| {
                fetch_count += 1;
                async move {
                    if page_token.is_none() {
                        Ok((vec![0; 300], Some("next-page".into())))
                    } else {
                        Ok((vec![1; 300], Some("still-next".into())))
                    }
                }
            },
            MAX_HISTORY_ITEMS_PER_COLLECTION,
            |mut page_items| {
                collected.append(&mut page_items);
                true
            },
        )
        .await
        .expect("history collection should truncate instead of failing");

        assert_eq!(collected.len(), MAX_HISTORY_ITEMS_PER_COLLECTION);
        assert!(truncated);
        assert_eq!(fetch_count, 2);
    }

    #[test]
    fn issue_history_keeps_the_newest_entries_with_global_truncation() {
        let entries = (0..=MAX_HISTORY_TOTAL_ITEMS)
            .map(|timestamp| operation_history_entry(&timestamp.to_string(), timestamp as i64))
            .collect();

        let response = finalize_issue_history_entries(entries, false);

        assert_eq!(response.entries.len(), MAX_HISTORY_TOTAL_ITEMS);
        assert!(response.truncated);
        assert_eq!(
            response.entries.first().map(|entry| entry.created_at_ms),
            Some(1_000)
        );
        assert_eq!(
            response.entries.last().map(|entry| entry.created_at_ms),
            Some(1)
        );
        assert!(
            response
                .entries
                .windows(2)
                .all(|pair| pair[0].created_at_ms >= pair[1].created_at_ms)
        );
    }
}
