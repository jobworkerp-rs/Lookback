//! Read-side helpers for the Issue Tracker management commands.

use std::future::Future;

use super::common::map_client_error;
use crate::{
    error::{AppError, AppResult},
    grpc::{
        issue_tracker::IssueTrackerClientError,
        proto::issue_tracker::{manager::v1 as manager, v1},
    },
};

const PAGE_SIZE: u32 = 100;
pub(super) const MAX_LIST_ITEMS: usize = 10_000;
const REVIEW_LIST_LIMIT_ERROR: &str = "Issue review list exceeded the safe client limit";

fn page_request(page_token: Option<String>) -> manager::PageRequest {
    manager::PageRequest {
        page_size: PAGE_SIZE,
        page_token,
    }
}

fn operation_history_args(
    filter: manager::OperationHistoryFilter,
    page_token: Option<String>,
) -> manager::ListOperationHistoryArgs {
    manager::ListOperationHistoryArgs {
        page: Some(page_request(page_token)),
        filter: Some(filter),
        include_total_size: Some(false),
        ..Default::default()
    }
}

async fn for_each_page_with_limit<T, F, Fut, V>(
    mut fetch: F,
    max_items: usize,
    truncate_at_limit: bool,
    mut visit: V,
) -> AppResult<bool>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = AppResult<(Vec<T>, Option<String>)>>,
    V: FnMut(Vec<T>) -> bool,
{
    let mut page_token = None;
    let mut item_count: usize = 0;
    loop {
        let (mut page_items, next_page_token) = fetch(page_token).await?;
        if item_count.saturating_add(page_items.len()) > max_items {
            if truncate_at_limit {
                page_items.truncate(max_items.saturating_sub(item_count));
                visit(page_items);
                return Ok(true);
            }
            return Err(AppError::IssueTrackerServer(
                "Issue Tracker list exceeded the safe client limit".into(),
            ));
        }
        item_count += page_items.len();
        if !visit(page_items) {
            return Ok(false);
        }
        let Some(next_page_token) = next_page_token.filter(|token| !token.is_empty()) else {
            return Ok(false);
        };
        if truncate_at_limit && item_count == max_items {
            return Ok(true);
        }
        page_token = Some(next_page_token);
    }
}

pub(super) async fn for_each_page<T, F, Fut, V>(fetch: F, visit: V) -> AppResult<()>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = AppResult<(Vec<T>, Option<String>)>>,
    V: FnMut(Vec<T>) -> bool,
{
    for_each_page_with_limit(fetch, MAX_LIST_ITEMS, false, visit).await?;
    Ok(())
}

pub(super) async fn collect_pages<T, F, Fut>(fetch: F) -> AppResult<Vec<T>>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = AppResult<(Vec<T>, Option<String>)>>,
{
    let mut items = Vec::new();
    for_each_page(fetch, |mut page_items| {
        items.append(&mut page_items);
        true
    })
    .await?;
    Ok(items)
}

pub(super) async fn list_candidates_all(
    service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
    correlation_id: &str,
    filter: manager::CandidateFilter,
) -> AppResult<Vec<manager::Candidate>> {
    collect_pages(|page_token| {
        let service = service.clone();
        let filter = filter.clone();
        async move {
            service
                .list_candidates(
                    correlation_id,
                    manager::ListCandidatesArgs {
                        page: Some(page_request(page_token)),
                        filter: Some(filter),
                        ..Default::default()
                    },
                )
                .await
                .map(|page| {
                    (
                        page.candidates,
                        page.page.and_then(|page| page.next_page_token),
                    )
                })
                .map_err(map_client_error)
        }
    })
    .await
}

/// Runs a full paged traversal, restarting it from the first page once when the
/// server reports that its composite snapshot expired mid-traversal. Only that
/// typed error is safe to recover; partial rows are never returned.
pub(super) async fn retry_once_on_expired_page_token<T, F, Fut>(mut traverse: F) -> AppResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, IssueTrackerClientError>>,
{
    let mut retried = false;
    loop {
        match traverse().await {
            Ok(value) => return Ok(value),
            Err(IssueTrackerClientError::Manager(error))
                if manager_error_is_page_token_expired(&error) && !retried =>
            {
                retried = true;
            }
            Err(error) => return Err(map_client_error(error)),
        }
    }
}

pub(super) async fn list_review_candidates_all(
    service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
    correlation_id: &str,
    filter: manager::ReviewCandidateFilter,
) -> AppResult<Vec<manager::ReviewCandidateRow>> {
    retry_once_on_expired_page_token(|| {
        collect_review_candidates_once(service, correlation_id, filter.clone())
    })
    .await
}

async fn collect_review_candidates_once(
    service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
    correlation_id: &str,
    filter: manager::ReviewCandidateFilter,
) -> Result<Vec<manager::ReviewCandidateRow>, IssueTrackerClientError> {
    let mut page_token = None;
    let mut rows = Vec::new();
    loop {
        let page = service
            .list_review_candidates(
                correlation_id,
                manager::ListReviewCandidatesArgs {
                    page: Some(page_request(page_token)),
                    filter: Some(filter.clone()),
                    ..Default::default()
                },
            )
            .await?;
        validate_review_page(&page)?;
        if rows.len().saturating_add(page.rows.len()) > MAX_LIST_ITEMS {
            return Err(IssueTrackerClientError::Transport(
                AppError::IssueTrackerServer(REVIEW_LIST_LIMIT_ERROR.into()),
            ));
        }
        rows.extend(page.rows);
        let Some(next_page_token) = page.page.and_then(|page| page.next_page_token) else {
            return Ok(rows);
        };
        if next_page_token.is_empty() {
            return Ok(rows);
        }
        page_token = Some(next_page_token);
    }
}

fn validate_review_page(
    page: &manager::ReviewCandidatePage,
) -> Result<(), IssueTrackerClientError> {
    let snapshot_ok = page
        .page
        .as_ref()
        .and_then(|page| page.snapshot.as_ref())
        .is_some()
        && page.composite_snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.backend_snapshot.is_some() && snapshot.manager_snapshot.is_some()
        });
    if !snapshot_ok {
        return Err(IssueTrackerClientError::Transport(
            AppError::IssueTrackerServer(
                "Issue Tracker review response is missing its composite snapshot".into(),
            ),
        ));
    }
    if page.rows.iter().any(|row| row.candidate.is_none()) {
        return Err(IssueTrackerClientError::Transport(
            AppError::IssueTrackerServer(
                "Issue Tracker review response contains a row without a Candidate".into(),
            ),
        ));
    }
    Ok(())
}

pub(super) fn manager_error_is_page_token_expired(error: &manager::ManagerError) -> bool {
    manager::ManagerErrorCode::try_from(error.code)
        .ok()
        .is_some_and(|code| code == manager::ManagerErrorCode::PageTokenExpired)
}

/// Maps UI lifecycle names to the server-side state filter. `Some(empty)`
/// means no filter; `None` means the request names only unknown states, so no
/// Issue can match and the server need not be asked.
pub(super) fn issue_state_filter(states: &[String]) -> Option<Vec<i32>> {
    use crate::grpc::proto::issue_tracker::v1::IssueState;
    if states.is_empty() {
        return Some(Vec::new());
    }
    let known = [
        IssueState::Open,
        IssueState::InProgress,
        IssueState::Blocked,
        IssueState::Deferred,
        IssueState::Done,
        IssueState::Canceled,
        IssueState::AwaitingReview,
    ]
    .into_iter()
    .filter(|state| {
        super::review::issue_state(*state as i32)
            .is_ok_and(|name| states.iter().any(|requested| requested == name))
    })
    .map(|state| state as i32)
    .collect::<Vec<_>>();
    (!known.is_empty()).then_some(known)
}

pub(super) async fn list_issues_all(
    service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
    correlation_id: &str,
    states: Vec<i32>,
) -> AppResult<Vec<manager::IssueWithSync>> {
    retry_once_on_expired_page_token(|| {
        collect_issues_once(service, correlation_id, states.clone())
    })
    .await
}

async fn collect_issues_once(
    service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
    correlation_id: &str,
    states: Vec<i32>,
) -> Result<Vec<manager::IssueWithSync>, IssueTrackerClientError> {
    let issue_filter = (!states.is_empty()).then(|| v1::IssueFilter {
        states,
        ..Default::default()
    });
    let mut page_token = None;
    let mut issues = Vec::new();
    loop {
        let page = service
            .list_issues_with_sync(
                correlation_id,
                manager::ListIssuesWithSyncArgs {
                    page: Some(page_request(page_token)),
                    issue_filter: issue_filter.clone(),
                    ..Default::default()
                },
            )
            .await?;
        if issues.len().saturating_add(page.issues.len()) > MAX_LIST_ITEMS {
            return Err(IssueTrackerClientError::Transport(
                AppError::IssueTrackerServer(
                    "Issue Tracker list exceeded the safe client limit".into(),
                ),
            ));
        }
        issues.extend(page.issues);
        match page.page.and_then(|page| page.next_page_token) {
            Some(token) if !token.is_empty() => page_token = Some(token),
            _ => return Ok(issues),
        }
    }
}

pub(super) async fn list_operation_history_capped(
    service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
    correlation_id: &str,
    filter: manager::OperationHistoryFilter,
    max_items: usize,
) -> AppResult<(Vec<manager::OperationHistoryEntry>, bool)> {
    let mut entries = Vec::new();
    let truncated = for_each_page_capped(
        |page_token| {
            let service = service.clone();
            let filter = filter.clone();
            async move {
                service
                    .list_operation_history(
                        correlation_id,
                        operation_history_args(filter, page_token),
                    )
                    .await
                    .map(|page| {
                        (
                            page.entries,
                            page.page.and_then(|page| page.next_page_token),
                        )
                    })
                    .map_err(map_client_error)
            }
        },
        max_items,
        |mut page_entries| {
            entries.append(&mut page_entries);
            true
        },
    )
    .await?;
    Ok((entries, truncated))
}

/// Capped visitors must not rely on short-circuit semantics because truncation ignores their return value.
pub(super) async fn for_each_page_capped<T, F, Fut, V>(
    fetch: F,
    max_items: usize,
    visit: V,
) -> AppResult<bool>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = AppResult<(Vec<T>, Option<String>)>>,
    V: FnMut(Vec<T>) -> bool,
{
    for_each_page_with_limit(fetch, max_items, true, visit).await
}

pub(super) async fn list_issue_operation_history_page(
    service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
    correlation_id: &str,
    filter: manager::OperationHistoryFilter,
    page_token: Option<String>,
) -> Result<manager::OperationHistoryPage, IssueTrackerClientError> {
    service
        .list_operation_history(correlation_id, operation_history_args(filter, page_token))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_page_token_expired_is_retryable() {
        let expired = manager::ManagerError {
            code: manager::ManagerErrorCode::PageTokenExpired as i32,
            ..Default::default()
        };
        assert!(manager_error_is_page_token_expired(&expired));
        for code in [
            manager::ManagerErrorCode::InvalidPageToken,
            manager::ManagerErrorCode::BackendUnavailable,
            manager::ManagerErrorCode::ManagerInvalidArgument,
        ] {
            assert!(!manager_error_is_page_token_expired(
                &manager::ManagerError {
                    code: code as i32,
                    ..Default::default()
                }
            ));
        }
    }

    #[test]
    fn operation_history_args_share_paging_and_count_policy() {
        let args = operation_history_args(
            manager::OperationHistoryFilter {
                method: Some("manager_transition_issue".into()),
                ..Default::default()
            },
            Some("opaque-cursor".into()),
        );

        assert_eq!(args.include_total_size, Some(false));
        assert_eq!(
            args.page
                .as_ref()
                .and_then(|page| page.page_token.as_deref()),
            Some("opaque-cursor")
        );
        assert_eq!(
            args.filter
                .as_ref()
                .and_then(|filter| filter.method.as_deref()),
            Some("manager_transition_issue")
        );
    }

    fn expired_token_error() -> IssueTrackerClientError {
        IssueTrackerClientError::Manager(Box::new(manager::ManagerError {
            code: manager::ManagerErrorCode::PageTokenExpired as i32,
            ..Default::default()
        }))
    }

    #[tokio::test]
    async fn expired_snapshot_restarts_the_traversal_once() {
        let mut attempts = 0;
        let rows = retry_once_on_expired_page_token(|| {
            attempts += 1;
            let attempt = attempts;
            async move {
                if attempt == 1 {
                    Err(expired_token_error())
                } else {
                    Ok(vec![1, 2])
                }
            }
        })
        .await
        .expect("the restarted traversal should succeed");
        assert_eq!(rows, vec![1, 2]);
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn a_second_expired_snapshot_is_returned_as_an_error() {
        let mut attempts = 0;
        let result: AppResult<Vec<i32>> = retry_once_on_expired_page_token(|| {
            attempts += 1;
            async { Err(expired_token_error()) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn other_traversal_errors_are_not_retried() {
        let mut attempts = 0;
        let result: AppResult<Vec<i32>> = retry_once_on_expired_page_token(|| {
            attempts += 1;
            async {
                Err(IssueTrackerClientError::InvalidRequest(
                    "bad request".to_owned(),
                ))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts, 1);
    }

    #[test]
    fn issue_state_filter_maps_known_states_and_rejects_an_unmatchable_request() {
        assert_eq!(issue_state_filter(&[]), Some(Vec::new()));
        assert_eq!(
            issue_state_filter(&["OPEN".into(), "AWAITING_REVIEW".into(), "BOGUS".into()]),
            Some(vec![
                crate::grpc::proto::issue_tracker::v1::IssueState::Open as i32,
                crate::grpc::proto::issue_tracker::v1::IssueState::AwaitingReview as i32,
            ])
        );
        // Only unknown names can never match a listed Issue.
        assert_eq!(issue_state_filter(&["BOGUS".into()]), None);
    }

    #[tokio::test]
    async fn collect_pages_accumulates_multiple_pages() {
        let items = collect_pages(|page_token| async move {
            if page_token.is_none() {
                Ok((vec![1, 2], Some("next-page".into())))
            } else {
                Ok((vec![3], None))
            }
        })
        .await
        .expect("all pages should be collected");

        assert_eq!(items, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn collect_pages_stops_on_an_empty_page_token() {
        let mut fetch_count = 0;
        let items = collect_pages(|_| {
            fetch_count += 1;
            async { Ok((vec![1], Some(String::new()))) }
        })
        .await
        .expect("an empty page token should end collection");

        assert_eq!(items, vec![1]);
        assert_eq!(fetch_count, 1);
    }

    #[tokio::test]
    async fn collect_pages_accepts_exactly_the_safe_client_limit() {
        let items = collect_pages(|_| async { Ok((vec![0; MAX_LIST_ITEMS], None)) })
            .await
            .expect("the safe client limit itself should be accepted");

        assert_eq!(items.len(), MAX_LIST_ITEMS);
    }

    #[tokio::test]
    async fn collect_pages_rejects_items_beyond_the_safe_client_limit() {
        let result = collect_pages(|page_token| async move {
            if page_token.is_none() {
                Ok((vec![0; MAX_LIST_ITEMS], Some("next-page".into())))
            } else {
                Ok((vec![1], None))
            }
        })
        .await;

        assert!(matches!(
            result,
            Err(AppError::IssueTrackerServer(message))
                if message == "Issue Tracker list exceeded the safe client limit"
        ));
    }

    #[test]
    fn review_page_requires_composite_snapshot_and_candidates() {
        let missing_snapshot = manager::ReviewCandidatePage::default();
        assert!(validate_review_page(&missing_snapshot).is_err());

        let valid_snapshot = manager::ReviewCandidatePage {
            page: Some(manager::PageInfo {
                snapshot: Some(manager::PageSnapshot::default()),
                ..Default::default()
            }),
            composite_snapshot: Some(manager::CompositePageSnapshot {
                backend_snapshot: Some(
                    crate::grpc::proto::issue_tracker::v1::PageSnapshot::default(),
                ),
                manager_snapshot: Some(manager::PageSnapshot::default()),
            }),
            ..Default::default()
        };
        assert!(validate_review_page(&valid_snapshot).is_ok());
        assert!(
            validate_review_page(&manager::ReviewCandidatePage {
                rows: vec![manager::ReviewCandidateRow::default()],
                ..valid_snapshot
            })
            .is_err()
        );
    }
}
