use serde::Serialize;
use tauri::State;

use crate::error::{AppError, AppResult};

use super::AppState;

use super::issues::issue_operation_journal::{PendingOperationJournal, PendingOperationRecord};

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PendingChatIssueOperation {
    pub operation_key: String,
    pub issue_id: String,
    pub method: String,
    pub created_at_ms: i64,
}

fn pending_chat_operations(records: Vec<PendingOperationRecord>) -> Vec<PendingChatIssueOperation> {
    records
        .into_iter()
        .filter(|record| {
            matches!(
                (record.method.as_str(), record.operation_key.as_str()),
                ("manager_create_issue", key) if key.starts_with("issue-chat-create-")
            ) || matches!(
                (record.method.as_str(), record.operation_key.as_str()),
                ("manager_update_issue", key) if key.starts_with("issue-chat-update-")
            ) || matches!(
                (record.method.as_str(), record.operation_key.as_str()),
                ("manager_apply_issue_admin_action", key) if key.starts_with("issue-chat-cancel-")
            )
        })
        .map(|record| PendingChatIssueOperation {
            operation_key: record.operation_key,
            issue_id: record.issue_id,
            method: record.method,
            created_at_ms: record.created_at_ms,
        })
        .collect()
}

#[tauri::command]
pub fn list_pending_chat_issue_operations(
    state: State<'_, AppState>,
) -> AppResult<Vec<PendingChatIssueOperation>> {
    let journal = PendingOperationJournal::new(state.data.issue_tracker_pending_operations_path());
    let records = journal
        .list()
        .map_err(|error| AppError::IssueTrackerServer(error.to_string()))?;
    Ok(pending_chat_operations(records))
}

#[tauri::command]
pub async fn retry_pending_chat_issue_operation(
    state: State<'_, AppState>,
    operation_key: String,
) -> AppResult<serde_json::Value> {
    if !operation_key.starts_with("issue-chat-") {
        return Err(AppError::Config("Not a chat Issue operation".into()));
    }
    let result =
        super::issue_chat_mutations::retry_pending_issue_chat_operation(&state, &operation_key)
            .await?;
    Ok(serde_json::json!({
        "issue_id": result.issue_id,
        "operation_id": result.operation_id,
        "operation_state": result.operation_state,
        "revision": result.revision.to_string(),
        "resulting_state": result.resulting_state,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_operation_listing_redacts_sensitive_payload_and_other_manager_writes() {
        let record = PendingOperationRecord::new(
            "issue-chat-update-key",
            "manager_update_issue",
            "issue-42",
            b"secret issue description",
            42,
            ("schema".into(), "root".into(), "contract".into()),
        );
        let other = PendingOperationRecord::new(
            "other",
            "manager_resolve_candidate",
            "issue-43",
            b"secret",
            43,
            ("schema".into(), "root".into(), "contract".into()),
        );
        let items = pending_chat_operations(vec![record, other]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].issue_id, "issue-42");
        let displayed = serde_json::to_string(&items).unwrap();
        assert!(!displayed.contains("secret"));
        assert!(!displayed.contains("payload"));
    }
}
