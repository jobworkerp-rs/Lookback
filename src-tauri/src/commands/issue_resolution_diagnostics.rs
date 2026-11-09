//! Surfaces Issue resolution failures reported by summary batches.
//!
//! The thread summary batch keeps resolution failures out of its own
//! success/failure counters and only lists them under
//! `issue_resolution_errors` in its terminal output. That output is logged as
//! one multi-kilobyte info line where repeated failures are easy to miss, so
//! this module condenses the list into a warn line.

use std::collections::BTreeSet;

use super::import::workflow_output_value;

const MESSAGE_CAP: usize = 240;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueResolutionErrorSummary {
    pub count: usize,
    pub thread_ids: Vec<String>,
    pub first_message: String,
}

pub(crate) fn summarize_issue_resolution_errors(raw: &str) -> Option<IssueResolutionErrorSummary> {
    let output = workflow_output_value(raw)?;
    let errors = output
        .get("issue_resolution_errors")
        .and_then(serde_json::Value::as_array)
        .filter(|errors| !errors.is_empty())?;
    let thread_ids = errors
        .iter()
        .filter_map(|error| error.get("thread_id").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let first_message = errors
        .iter()
        .find_map(|error| error.get("message").and_then(serde_json::Value::as_str))
        .map(|message| message.chars().take(MESSAGE_CAP).collect::<String>())
        .unwrap_or_default();
    Some(IssueResolutionErrorSummary {
        count: errors.len(),
        thread_ids,
        first_message,
    })
}

/// Logs one warn line per terminal batch output that carries resolution errors.
pub(crate) fn warn_issue_resolution_errors(raw: &str) {
    if let Some(summary) = summarize_issue_resolution_errors(raw) {
        tracing::warn!(
            target: "issue_resolution",
            count = summary.count,
            thread_ids = ?summary.thread_ids,
            first_message = %summary.first_message,
            "thread issue resolution failed in summary batch"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch_chunk(output: serde_json::Value) -> String {
        serde_json::json!({
            "id": "job-1",
            "output": output.to_string(),
            "position": "/ROOT/do/9",
            "status": "Completed"
        })
        .to_string()
    }

    #[test]
    fn summarizes_errors_nested_in_a_workflow_result_chunk() {
        let raw = batch_chunk(serde_json::json!({
            "failed_count": 0,
            "issue_resolution_errors": [
                { "thread_id": "2", "status": "UNKNOWN", "message": "Review candidate page is incomplete" },
                { "thread_id": "1", "status": "UNKNOWN", "message": "second" },
                { "thread_id": "2", "status": "UNKNOWN", "message": "third" }
            ]
        }));
        let summary = summarize_issue_resolution_errors(&raw).expect("errors are summarized");
        assert_eq!(summary.count, 3);
        assert_eq!(summary.thread_ids, vec!["1".to_string(), "2".to_string()]);
        assert_eq!(summary.first_message, "Review candidate page is incomplete");
    }

    #[test]
    fn summarizes_a_bare_output_object() {
        let raw = serde_json::json!({
            "issue_resolution_errors": [{ "thread_id": "9", "message": "x" }]
        })
        .to_string();
        let summary = summarize_issue_resolution_errors(&raw).expect("bare output is accepted");
        assert_eq!(summary.count, 1);
        assert_eq!(summary.thread_ids, vec!["9".to_string()]);
    }

    #[test]
    fn caps_the_first_message_length() {
        let raw = batch_chunk(serde_json::json!({
            "issue_resolution_errors": [{ "thread_id": "1", "message": "x".repeat(1000) }]
        }));
        let summary = summarize_issue_resolution_errors(&raw).expect("summarized");
        assert_eq!(summary.first_message.chars().count(), MESSAGE_CAP);
    }

    #[test]
    fn ignores_outputs_without_resolution_errors() {
        assert!(
            summarize_issue_resolution_errors(&batch_chunk(serde_json::json!({
                "issue_resolution_errors": []
            })))
            .is_none()
        );
        assert!(
            summarize_issue_resolution_errors(&batch_chunk(serde_json::json!({
                "failed_count": 0
            })))
            .is_none()
        );
        assert!(summarize_issue_resolution_errors("not json").is_none());
        assert!(summarize_issue_resolution_errors("").is_none());
    }

    #[test]
    fn warn_does_not_panic_on_arbitrary_input() {
        warn_issue_resolution_errors("{\"output\": 12}");
        warn_issue_resolution_errors(&batch_chunk(serde_json::json!({
            "issue_resolution_errors": [{ "message": 5 }]
        })));
    }
}
