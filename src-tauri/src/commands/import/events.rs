use serde::Deserialize;

use super::super::{AppError, AppResult};
use super::guards::ImportSource;
use super::pending::{PendingImportScope, PendingImportState};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(super) struct ImportCreationEventWire {
    pub(super) version: u32,
    pub(super) event: String,
    pub(super) source: ImportSource,
    pub(super) session_key: String,
    pub(super) thread_id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(super) struct ImportSessionCompletedEvent {
    pub(super) source: ImportSource,
    pub(super) session_key: String,
    pub(super) thread_id: String,
    pub(super) imported_count: u64,
    pub(super) success: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(super) struct ImportCompletedError {
    pub(super) code: Option<String>,
    pub(super) message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(super) struct ImportCompletedSession {
    pub(super) session_key: Option<String>,
    pub(super) status: String,
    pub(super) imported_count: u64,
    pub(super) error: Option<ImportCompletedError>,
}

/// Fields mirror the full wire contract even when control flow does not read version, event, counts, or imported_count.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(super) struct ImportCompletedEvent {
    pub(super) version: u32,
    pub(super) event: String,
    pub(super) sessions_processed: u64,
    pub(super) threads_created: u64,
    pub(super) memories_imported: u64,
    pub(super) memories_skipped_duplicate: u64,
    pub(super) memories_skipped_ignored: u64,
    pub(super) errors_count: u64,
    pub(super) success: bool,
    pub(super) sessions: Vec<ImportCompletedSession>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ImportStdoutLine {
    Creation(ImportCreationEventWire),
    SessionCompleted(ImportSessionCompletedEvent),
    ImportCompleted(ImportCompletedEvent),
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompletionDisposition {
    TrackPending,
    IgnoreExisting,
}

pub(super) fn classify_session_completion(
    pending: &PendingImportState,
    scope: &PendingImportScope,
    completed: &ImportSessionCompletedEvent,
) -> AppResult<CompletionDisposition> {
    if !completed.success {
        return Err(AppError::Config(format!(
            "memories-import session failed: {}/{}",
            completed.source.cli_subcommand(),
            completed.session_key
        )));
    }
    let pending_for_key = pending.sessions.iter().find(|session| {
        scope.contains(session)
            && session.source == completed.source
            && session.session_key == completed.session_key
    });
    match pending_for_key {
        Some(session) if session.thread_id == completed.thread_id => {
            Ok(CompletionDisposition::TrackPending)
        }
        Some(_) => Err(AppError::Config(format!(
            "memories-import changed thread_id for pending session {}",
            completed.session_key
        ))),
        None => Ok(CompletionDisposition::IgnoreExisting),
    }
}

pub(super) fn parse_import_stdout_line(line: &str) -> AppResult<ImportStdoutLine> {
    let value = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(value) => value,
        Err(error) if line.contains("memories-import-event") => {
            return Err(AppError::Config(format!(
                "malformed memories-import event: {error}"
            )));
        }
        Err(_) => return Ok(ImportStdoutLine::Text(line.to_string())),
    };
    if value.get("schema").and_then(|value| value.as_str()) != Some("memories-import-event") {
        return Ok(ImportStdoutLine::Text(line.to_string()));
    }
    let version = value.get("version").and_then(|value| value.as_u64());
    let event_name = value.get("event").and_then(|value| value.as_str());
    if version != Some(1) {
        return Err(AppError::Config(format!(
            "unsupported memories-import event version {:?}",
            version
        )));
    }
    if event_name == Some("session_completed") {
        let event: ImportSessionCompletedEvent = serde_json::from_value(value)
            .map_err(|error| AppError::Config(format!("invalid memories-import event: {error}")))?;
        validate_import_event_identity(&event.session_key, &event.thread_id)?;
        return Ok(ImportStdoutLine::SessionCompleted(event));
    }
    if event_name == Some("import_completed") {
        let event: ImportCompletedEvent = serde_json::from_value(value)
            .map_err(|error| AppError::Config(format!("invalid memories-import event: {error}")))?;
        return Ok(ImportStdoutLine::ImportCompleted(event));
    }
    let event: ImportCreationEventWire = serde_json::from_value(value)
        .map_err(|error| AppError::Config(format!("invalid memories-import event: {error}")))?;
    if event.event != "thread_created" {
        return Err(AppError::Config(format!(
            "unsupported memories-import event {}@{}",
            event.event, event.version
        )));
    }
    validate_import_event_identity(&event.session_key, &event.thread_id)?;
    Ok(ImportStdoutLine::Creation(event))
}

pub(super) fn reported_imported_count(lines: &[String]) -> Option<usize> {
    lines.iter().find_map(|line| {
        line.trim()
            .strip_prefix("Memories imported:")
            .and_then(|count| count.trim().parse().ok())
    })
}

fn validate_import_event_identity(session_key: &str, thread_id: &str) -> AppResult<()> {
    if session_key.trim().is_empty() {
        return Err(AppError::Config(
            "memories-import event session_key must not be empty".into(),
        ));
    }
    let parsed_id = thread_id.parse::<i64>().map_err(|_| {
        AppError::Config("memories-import event thread_id must be a decimal i64 string".into())
    })?;
    if parsed_id <= 0 || thread_id != parsed_id.to_string() {
        return Err(AppError::Config(
            "memories-import event thread_id must be a canonical positive decimal string".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_import_event_source(
    expected_source: ImportSource,
    event_source: ImportSource,
) -> AppResult<()> {
    if event_source == expected_source {
        return Ok(());
    }
    Err(AppError::Config(format!(
        "memories-import lifecycle event source mismatch: expected {}, got {}",
        expected_source.cli_subcommand(),
        event_source.cli_subcommand()
    )))
}

#[cfg(test)]
mod tests {
    use super::super::guards::ImportSource;
    use super::super::pending::{PENDING_IMPORT_FORMAT_VERSION, PendingImportState};
    use super::super::tests_common::{local_scope, pending_session};
    use super::*;

    #[test]
    fn importer_jsonl_parser_separates_events_from_text_and_preserves_large_ids() {
        let line = r#"{"schema":"memories-import-event","version":1,"event":"thread_created","source":"codex","session_key":"session/a","thread_id":"9007199254740993"}"#;
        assert_eq!(
            parse_import_stdout_line(line).unwrap(),
            ImportStdoutLine::Creation(ImportCreationEventWire {
                version: 1,
                event: "thread_created".into(),
                source: ImportSource::Codex,
                session_key: "session/a".into(),
                thread_id: "9007199254740993".into(),
            })
        );
        assert_eq!(
            parse_import_stdout_line("codex summary:").unwrap(),
            ImportStdoutLine::Text("codex summary:".into())
        );
    }

    #[test]
    fn importer_summary_exposes_missing_event_protocol_without_parsing_event_lines_as_text() {
        assert_eq!(
            reported_imported_count(&["codex summary:".into(), "  Memories imported: 3".into(),]),
            Some(3)
        );
        assert_eq!(reported_imported_count(&["unrelated".into()]), None);
    }

    #[test]
    fn importer_session_completion_is_versioned_and_typed() {
        let line = r#"{"schema":"memories-import-event","version":1,"event":"session_completed","source":"claude-code","session_key":"project/session","thread_id":"42","imported_count":3,"success":true}"#;
        assert_eq!(
            parse_import_stdout_line(line).unwrap(),
            ImportStdoutLine::SessionCompleted(ImportSessionCompletedEvent {
                source: ImportSource::ClaudeCode,
                session_key: "project/session".into(),
                thread_id: "42".into(),
                imported_count: 3,
                success: true,
            })
        );
        assert!(parse_import_stdout_line(&line.replace("\"42\"", "42")).is_err());
        assert!(
            parse_import_stdout_line(
                r#"{"schema":"memories-import-event","version":1,"event":"thread_created""#
            )
            .is_err()
        );
    }

    #[test]
    fn existing_non_pending_completion_is_success_but_not_a_generation_target() {
        let scope = local_scope(1);
        let completion = ImportSessionCompletedEvent {
            source: ImportSource::Codex,
            session_key: "existing".into(),
            thread_id: "42".into(),
            imported_count: 3,
            success: true,
        };
        assert_eq!(
            classify_session_completion(&PendingImportState::default(), &scope, &completion)
                .unwrap(),
            CompletionDisposition::IgnoreExisting
        );
        let pending = PendingImportState {
            format_version: PENDING_IMPORT_FORMAT_VERSION,
            sessions: vec![pending_session(
                &scope,
                ImportSource::Codex,
                "existing",
                "42",
            )],
        };
        assert_eq!(
            classify_session_completion(&pending, &scope, &completion).unwrap(),
            CompletionDisposition::TrackPending
        );
        let mismatched = ImportSessionCompletedEvent {
            thread_id: "43".into(),
            ..completion
        };
        assert!(classify_session_completion(&pending, &scope, &mismatched).is_err());
    }

    #[test]
    fn importer_completion_event_preserves_counts_sessions_and_optional_errors() {
        let line = r#"{"schema":"memories-import-event","version":1,"event":"import_completed","sessions_processed":87,"threads_created":72,"memories_imported":0,"memories_skipped_duplicate":13036,"memories_skipped_ignored":4977,"errors_count":1,"success":false,"sessions":[{"session_key":"ses_completed","status":"completed","imported_count":0},{"session_key":null,"status":"failed","imported_count":0,"error":{"code":"parse_error","message":"could not parse session"}},{"session_key":"ses_code_only","status":"failed","imported_count":2,"error":{"code":"transport"}}]}"#;

        assert_eq!(
            parse_import_stdout_line(line).unwrap(),
            ImportStdoutLine::ImportCompleted(ImportCompletedEvent {
                version: 1,
                event: "import_completed".into(),
                sessions_processed: 87,
                threads_created: 72,
                memories_imported: 0,
                memories_skipped_duplicate: 13036,
                memories_skipped_ignored: 4977,
                errors_count: 1,
                success: false,
                sessions: vec![
                    ImportCompletedSession {
                        session_key: Some("ses_completed".into()),
                        status: "completed".into(),
                        imported_count: 0,
                        error: None,
                    },
                    ImportCompletedSession {
                        session_key: None,
                        status: "failed".into(),
                        imported_count: 0,
                        error: Some(ImportCompletedError {
                            code: Some("parse_error".into()),
                            message: Some("could not parse session".into()),
                        }),
                    },
                    ImportCompletedSession {
                        session_key: Some("ses_code_only".into()),
                        status: "failed".into(),
                        imported_count: 2,
                        error: Some(ImportCompletedError {
                            code: Some("transport".into()),
                            message: None,
                        }),
                    },
                ],
            })
        );
    }

    #[test]
    fn importer_unknown_versioned_event_still_fails_as_unsupported() {
        let error = parse_import_stdout_line(
                r#"{"schema":"memories-import-event","version":1,"event":"import_started","source":"codex","session_key":"session","thread_id":"42"}"#,
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported memories-import event import_started@1"));
    }
}
