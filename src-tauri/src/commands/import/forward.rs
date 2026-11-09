use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tauri::AppHandle;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{info, warn};

use super::super::{StepOutcome, StepStatus};
use super::aggregator::{AggregatedState, ChildOutcome, ImportAggregator};
use super::events::{
    CompletionDisposition, ImportCompletedEvent, ImportSessionCompletedEvent, ImportStdoutLine,
    classify_session_completion, parse_import_stdout_line, reported_imported_count,
    validate_import_event_source,
};
use super::guards::ImportSource;
use super::pending::{PendingImportScope, PendingImportSession, PendingImportState};
use super::pending_store::record_pending_import;
use super::request::ImportStep;
use super::step_runner::{emit_step, extract_summary_block};

#[derive(Clone)]
pub(super) struct ImportLogContext {
    pub(super) aggregator: Arc<ImportAggregator>,
    pub(super) pending_path: PathBuf,
    pub(super) pending_imports: Arc<parking_lot::Mutex<PendingImportState>>,
    pub(super) completed_sessions: Arc<parking_lot::Mutex<Vec<ImportSessionCompletedEvent>>>,
    pub(super) pending_scope: PendingImportScope,
    pub(super) expected_source: ImportSource,
    pub(super) dry_run: bool,
    pub(super) cancel: tokio_util::sync::CancellationToken,
    /// Native tracing scope shared by the sequential source children.
    pub(super) native_log_dir: Option<PathBuf>,
}

pub(super) async fn run_one_source(
    mut cmd: Command,
    context: ImportLogContext,
) -> Option<AggregatedState> {
    if context.cancel.is_cancelled() {
        return None;
    }
    let bin_for_msg = cmd.as_std().get_program().to_string_lossy().into_owned();
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let outcome = ChildOutcome::Failure(format!("spawn {bin_for_msg} failed: {e}"));
            return context.aggregator.record(outcome);
        }
    };
    if let Some(pid) = child.id()
        && let Some(path) = context.native_log_dir.as_deref()
        && let Err(error) = crate::log_rotation::mark_native_log_dir_pid(path, pid)
    {
        tracing::debug!(%error, pid, path = %path.display(), "could not record native import child PID");
    }
    forward_import_logs(child, context).await
}

const DRY_RUN_LIFECYCLE_EVENT_ERROR: &str =
    "memories-import emitted lifecycle event during dry-run";

/// Pipe CLI output to tracing, await child, and feed the per-child outcome
/// into the aggregator. Terminal `thread-import` step is emitted from the
/// caller after the source loop finishes (see `start_import`).
async fn forward_import_logs(
    mut child: tokio::process::Child,
    context: ImportLogContext,
) -> Option<AggregatedState> {
    let stdout_lines = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let event_error = Arc::new(parking_lot::Mutex::new(None::<String>));
    let import_completed_event = Arc::new(parking_lot::Mutex::new(None::<ImportCompletedEvent>));
    let created_sessions = Arc::new(parking_lot::Mutex::new(Vec::<PendingImportSession>::new()));
    let completed_in_child = Arc::new(parking_lot::Mutex::new(
        Vec::<ImportSessionCompletedEvent>::new(),
    ));
    let session_event_count = Arc::new(AtomicUsize::new(0));
    let event_ingestion = Arc::new(tokio::sync::Mutex::new(()));
    let pending_path = context.pending_path.clone();
    let pending_imports = context.pending_imports.clone();
    let completed_sessions = context.completed_sessions.clone();
    let pending_scope = context.pending_scope.clone();
    let expected_source = context.expected_source;
    let dry_run = context.dry_run;
    let cancel = context.cancel.clone();
    let mut stdout_reader = child.stdout.take().map(|stdout| {
        let lines = stdout_lines.clone();
        let event_error = event_error.clone();
        let import_completed_event = import_completed_event.clone();
        let created_sessions = created_sessions.clone();
        let completed_in_child = completed_in_child.clone();
        let session_event_count = session_event_count.clone();
        let pending_scope = pending_scope.clone();
        let event_ingestion = event_ingestion.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                match parse_import_stdout_line(&line) {
                    Ok(ImportStdoutLine::Creation(event)) => {
                        if dry_run {
                            *event_error.lock() = Some(DRY_RUN_LIFECYCLE_EVENT_ERROR.into());
                            continue;
                        }
                        if let Err(error) = validate_import_event_source(expected_source, event.source)
                        {
                            *event_error.lock() = Some(error.to_string());
                            continue;
                        }
                        let _ingestion = event_ingestion.lock().await;
                        if cancel.is_cancelled() {
                            continue;
                        }
                        let session = PendingImportSession {
                            memories_endpoint_id: pending_scope.memories_endpoint_id.clone(),
                            user_id: pending_scope.user_id,
                            source: event.source,
                            session_key: event.session_key,
                            thread_id: event.thread_id,
                            lifecycle_id: None,
                            plan: None,
                            steps: BTreeMap::new(),
                        };
                        let result = record_pending_import(
                            &pending_path,
                            &mut pending_imports.lock(),
                            session.clone(),
                        );
                        if let Err(error) = result {
                            *event_error.lock() = Some(error.to_string());
                            continue;
                        }
                        created_sessions.lock().push(session);
                    }
                    Ok(ImportStdoutLine::SessionCompleted(completed)) => {
                        if dry_run {
                            *event_error.lock() = Some(DRY_RUN_LIFECYCLE_EVENT_ERROR.into());
                            continue;
                        }
                        if let Err(error) =
                            validate_import_event_source(expected_source, completed.source)
                        {
                            *event_error.lock() = Some(error.to_string());
                            continue;
                        }
                        let _ingestion = event_ingestion.lock().await;
                        if cancel.is_cancelled() {
                            continue;
                        }
                        session_event_count.fetch_add(1, Ordering::AcqRel);
                        match classify_session_completion(
                            &pending_imports.lock(),
                            &pending_scope,
                            &completed,
                        ) {
                            Ok(CompletionDisposition::TrackPending) => {
                                completed_in_child.lock().push(completed.clone());
                                completed_sessions.lock().push(completed);
                            }
                            Ok(CompletionDisposition::IgnoreExisting) => {
                                info!(
                                    target: "memories-import",
                                    source = completed.source.cli_subcommand(),
                                    session_key = completed.session_key,
                                    "existing non-pending session completed; excluding it from generation"
                                );
                            }
                            Err(error) => *event_error.lock() = Some(error.to_string()),
                        }
                    }
                    Ok(ImportStdoutLine::ImportCompleted(completed)) => {
                        *import_completed_event.lock() = Some(completed);
                    }
                    Ok(ImportStdoutLine::Text(line)) => {
                        info!(target: "memories-import", "{line}");
                        lines.lock().push(line);
                    }
                    Err(error) => {
                        *event_error.lock() = Some(error.to_string());
                    }
                }
            }
        })
    });
    // Keep the stderr tail so a non-zero exit can surface *why* it failed
    // (e.g. a TLS handshake error against a remote server). Without this the
    // UI only ever saw `exit status: 1` and the cause lived in stderr alone.
    let stderr_lines = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let mut stderr_reader = child.stderr.take().map(|stderr| {
        let lines = stderr_lines.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                warn!(target: "memories-import", "{line}");
                lines.lock().push(line);
            }
        })
    });

    let status = tokio::select! {
        status = child.wait() => Some(status),
        _ = cancel.cancelled() => {
            // Serializing cancellation with event ingestion prevents a buffered
            // lifecycle line from mutating pending state after this import ends.
            let _ingestion = event_ingestion.lock().await;
            abort_and_join_output_reader(&mut stdout_reader).await;
            abort_and_join_output_reader(&mut stderr_reader).await;
            if let Err(error) = child.start_kill() {
                warn!(?error, "failed to kill cancelled memories-import child");
            }
            if let Err(error) = child.wait().await {
                warn!(?error, "failed to wait for cancelled memories-import child");
            }
            None
        }
    };
    let status = status?;
    if let Some(handle) = stdout_reader {
        let _ = handle.await;
    }
    if let Some(handle) = stderr_reader {
        let _ = handle.await;
    }

    let event_failure = event_error.lock().clone();
    let raw_event_error = event_failure.clone();
    let missing_completion = created_sessions
        .lock()
        .iter()
        .find(|created| {
            !completed_in_child.lock().iter().any(|completed| {
                completed.source == created.source
                    && completed.session_key == created.session_key
                    && completed.thread_id == created.thread_id
            })
        })
        .cloned();
    let event_failure = event_failure.or_else(|| {
        missing_completion.map(|missing| {
            format!(
                "memories-import omitted session_completed for {}/{}",
                missing.source.cli_subcommand(),
                missing.session_key
            )
        })
    });
    let event_failure = event_failure.or_else(|| {
        let reported = reported_imported_count(&stdout_lines.lock()).unwrap_or_default();
        (!dry_run && reported > 0 && session_event_count.load(Ordering::Acquire) == 0)
            .then(|| "memories-import wrote memories without versioned session events".to_string())
    });
    let outcome = decide_child_outcome(
        import_completed_event.lock().clone(),
        status,
        event_failure,
        raw_event_error,
        &stdout_lines.lock(),
        &stderr_lines.lock(),
    );

    context.aggregator.record(outcome)
}

fn decide_child_outcome(
    import_completed_event: Option<ImportCompletedEvent>,
    status: Result<std::process::ExitStatus, std::io::Error>,
    event_failure: Option<String>,
    raw_event_error: Option<String>,
    stdout_lines: &[String],
    stderr_lines: &[String],
) -> ChildOutcome {
    if let Some(completed) = import_completed_event.as_ref() {
        if let Some(error) = event_failure.as_ref() {
            let error_kind = if raw_event_error.is_some() {
                "event"
            } else {
                "protocol"
            };
            warn!(
                %error,
                error_kind,
                "memories-import emitted an event or protocol error despite import_completed"
            );
        }
        let summary = extract_summary_block(stdout_lines);
        if completed.success {
            match event_failure.as_deref() {
                Some(error) => ChildOutcome::PartialSuccess {
                    summary,
                    warning: import_completed_partial_warning(
                        completed,
                        Some(error),
                        raw_event_error.as_deref(),
                    ),
                },
                None => {
                    info!("memories-import completed successfully");
                    ChildOutcome::Success { summary }
                }
            }
        } else {
            let failed_sessions = import_completed_failure_details(completed);
            // The current status domain is {completed, failed}; anything else
            // is treated as non-completed until the contract defines it.
            if completed
                .sessions
                .iter()
                .any(|session| session.status == "completed")
            {
                ChildOutcome::PartialSuccess {
                    summary,
                    warning: import_completed_partial_warning(
                        completed,
                        event_failure.as_deref(),
                        raw_event_error.as_deref(),
                    ),
                }
            } else {
                ChildOutcome::Failure(import_completed_failure_message(
                    completed,
                    &failed_sessions,
                ))
            }
        }
    } else {
        match (status, event_failure) {
            (_, Some(error)) => ChildOutcome::Failure(error),
            (Ok(s), None) if s.success() => {
                info!("memories-import succeeded");
                let summary = extract_summary_block(stdout_lines);
                ChildOutcome::Success { summary }
            }
            (Ok(s), None) => {
                warn!(?s, "memories-import exited non-zero");
                ChildOutcome::Failure(failure_message(&format!("exit {s}"), stderr_lines))
            }
            (Err(e), None) => {
                warn!(error = ?e, "memories-import wait failed");
                ChildOutcome::Failure(format!("wait: {e}"))
            }
        }
    }
}

async fn abort_and_join_output_reader(reader: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(handle) = reader.take() {
        handle.abort();
        let _ = handle.await;
    }
}

/// Number of trailing stderr lines folded into a failure message. Enough to
/// carry a gRPC connect / TLS error (usually 1-2 lines) without dumping a full
/// backtrace into the toast.
const STDERR_TAIL_LINES: usize = 5;

/// Build the user-facing failure string: the exit reason plus the tail of the
/// child's stderr, when it produced any. Pure so the formatting is unit-tested
/// without spawning a process.
fn failure_message(reason: &str, stderr_lines: &[String]) -> String {
    let tail: Vec<&str> = stderr_lines
        .iter()
        .rev()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .take(STDERR_TAIL_LINES)
        .collect();
    if tail.is_empty() {
        return reason.to_string();
    }
    let tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
    format!("{reason}\n{tail}")
}

fn import_completed_failure_details(event: &ImportCompletedEvent) -> Vec<String> {
    event
        .sessions
        .iter()
        .filter(|session| session.status == "failed")
        .map(|session| {
            let session_key = session
                .session_key
                .as_deref()
                .unwrap_or("<unknown session>");
            let error = session
                .error
                .as_ref()
                .and_then(|error| match (&error.code, &error.message) {
                    (Some(code), Some(message)) => Some(format!("{code}: {message}")),
                    (Some(code), None) => Some(code.clone()),
                    (None, Some(message)) => Some(message.clone()),
                    (None, None) => None,
                })
                .unwrap_or_else(|| "unknown error".into());
            format!("{session_key}: {error}")
        })
        .collect()
}

fn import_completed_warning(event: &ImportCompletedEvent, failed_sessions: &[String]) -> String {
    let details = failed_sessions.join("; ");
    if details.is_empty() {
        format!(
            "memories-import completed with errors_count={}",
            event.errors_count
        )
    } else {
        format!(
            "memories-import completed with errors_count={}: {details}",
            event.errors_count
        )
    }
}

fn append_import_completed_warning(
    mut warning: String,
    event_failure: Option<&str>,
    raw_event_error: Option<&str>,
) -> String {
    for error in [event_failure, raw_event_error].into_iter().flatten() {
        if error.is_empty() || warning.contains(error) {
            continue;
        }
        if !warning.is_empty() {
            warning.push_str("; ");
        }
        warning.push_str(error);
    }
    warning
}

fn import_completed_partial_warning(
    completed: &ImportCompletedEvent,
    event_failure: Option<&str>,
    raw_event_error: Option<&str>,
) -> String {
    let warning = if completed.success {
        String::new()
    } else {
        import_completed_warning(completed, &import_completed_failure_details(completed))
    };
    append_import_completed_warning(warning, event_failure, raw_event_error)
}

fn import_completed_failure_message(
    event: &ImportCompletedEvent,
    failed_sessions: &[String],
) -> String {
    let details = failed_sessions.join("; ");
    if details.is_empty() {
        format!(
            "memories-import failed with errors_count={}",
            event.errors_count
        )
    } else {
        format!("memories-import failed: {details}")
    }
}

pub(super) fn import_state_allows_downstream(state: &AggregatedState) -> bool {
    matches!(
        state,
        AggregatedState::AllSucceeded { .. } | AggregatedState::PartialSucceeded { .. }
    )
}

pub(super) fn import_terminal_step_outcome(state: AggregatedState, dry_run: bool) -> StepOutcome {
    match state {
        AggregatedState::AnyFailed(failures) => StepOutcome {
            status: StepStatus::Failed,
            message: Some(failures.join("; ")),
        },
        AggregatedState::PartialSucceeded {
            summaries,
            warnings,
        } => {
            let mut message = warnings.join("; ");
            if dry_run && !summaries.is_empty() {
                message.push_str("\n\n");
                message.push_str(&summaries.join("\n\n"));
            }
            StepOutcome {
                status: StepStatus::Warning,
                message: Some(message),
            }
        }
        AggregatedState::AllSucceeded { summaries } => StepOutcome {
            status: StepStatus::Done,
            message: (dry_run && !summaries.is_empty()).then(|| summaries.join("\n\n")),
        },
    }
}

pub(super) fn emit_thread_import_terminal(
    app: &AppHandle,
    job_id: &str,
    state: AggregatedState,
    dry_run: bool,
) {
    let outcome = import_terminal_step_outcome(state, dry_run);
    emit_step(
        app,
        job_id,
        ImportStep::ThreadImport,
        outcome.status,
        outcome.message,
    );
}

#[cfg(test)]
mod tests {
    use super::super::aggregator::{AggregatedState, ChildOutcome, ImportAggregator};
    use super::super::events::{
        ImportCompletedError, ImportCompletedEvent, ImportCompletedSession,
        ImportSessionCompletedEvent,
    };
    use super::super::guards::ImportSource;
    use super::super::pending::PendingImportState;
    use super::super::pending_store::{
        completed_pending_state, load_pending_imports, record_pending_import, save_pending_imports,
        select_targeted_sessions,
    };
    use super::super::tests_common::{local_scope, pending_session};
    use super::*;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::Arc;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone)]
    struct WarningCapture(Arc<parking_lot::Mutex<Vec<String>>>);

    struct EventFieldVisitor {
        fields: Vec<String>,
    }

    impl tracing::field::Visit for EventFieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.fields.push(format!("{}={value:?}", field.name()));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.fields.push(format!("{}={value}", field.name()));
        }
    }

    impl<S> tracing_subscriber::Layer<S> for WarningCapture
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() != tracing::Level::WARN {
                return;
            }
            let mut visitor = EventFieldVisitor { fields: Vec::new() };
            event.record(&mut visitor);
            self.0.lock().push(visitor.fields.join(" "));
        }
    }

    fn warning_capture() -> (
        Arc<parking_lot::Mutex<Vec<String>>>,
        impl tracing::Subscriber,
    ) {
        let warnings = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(WarningCapture(warnings.clone()));
        (warnings, subscriber)
    }

    fn import_log_context(
        aggregator: Arc<ImportAggregator>,
        pending_path: PathBuf,
        pending_imports: Arc<parking_lot::Mutex<PendingImportState>>,
        completed_sessions: Arc<parking_lot::Mutex<Vec<ImportSessionCompletedEvent>>>,
        pending_scope: super::super::pending::PendingImportScope,
    ) -> ImportLogContext {
        ImportLogContext {
            aggregator,
            pending_path,
            pending_imports,
            completed_sessions,
            pending_scope,
            expected_source: ImportSource::ClaudeCode,
            dry_run: false,
            cancel: tokio_util::sync::CancellationToken::new(),
            native_log_dir: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_scripted_import(
        script: &str,
        aggregator: Arc<ImportAggregator>,
        pending_path: PathBuf,
        pending_imports: Arc<parking_lot::Mutex<PendingImportState>>,
        completed_sessions: Arc<parking_lot::Mutex<Vec<ImportSessionCompletedEvent>>>,
        pending_scope: super::super::pending::PendingImportScope,
        dry_run: bool,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Option<AggregatedState> {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        forward_import_logs(
            command.spawn().unwrap(),
            ImportLogContext {
                dry_run,
                cancel,
                ..import_log_context(
                    aggregator,
                    pending_path,
                    pending_imports,
                    completed_sessions,
                    pending_scope,
                )
            },
        )
        .await
    }

    #[test]
    fn import_completed_partial_warning_uses_success_and_failure_bases() {
        let success = ImportCompletedEvent {
            version: 1,
            event: "import_completed".into(),
            sessions_processed: 1,
            threads_created: 1,
            memories_imported: 1,
            memories_skipped_duplicate: 0,
            memories_skipped_ignored: 0,
            errors_count: 2,
            success: true,
            sessions: vec![ImportCompletedSession {
                session_key: Some("ignored-failed-session".into()),
                status: "failed".into(),
                imported_count: 0,
                error: None,
            }],
        };
        assert_eq!(
            import_completed_partial_warning(
                &success,
                Some("event failure"),
                Some("raw event failure")
            ),
            "event failure; raw event failure"
        );

        let failed = ImportCompletedEvent {
            success: false,
            sessions: vec![ImportCompletedSession {
                session_key: Some("failed-session".into()),
                status: "failed".into(),
                imported_count: 0,
                error: Some(ImportCompletedError {
                    code: Some("parse_error".into()),
                    message: Some("broken".into()),
                }),
            }],
            ..success.clone()
        };
        assert_eq!(
            import_completed_partial_warning(
                &failed,
                Some("event failure"),
                Some("raw event failure")
            ),
            "memories-import completed with errors_count=2: failed-session: parse_error: broken; event failure; raw event failure"
        );
    }

    #[cfg(unix)]
    #[test]
    fn child_outcome_wait_error_is_a_failure_with_wait_context() {
        let outcome = decide_child_outcome(
            None,
            Err(std::io::Error::other("wait broke")),
            None,
            None,
            &[],
            &[],
        );

        assert!(matches!(
            outcome,
            ChildOutcome::Failure(message) if message.contains("wait: wait broke")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn successful_completion_event_returns_success_with_extracted_summary() {
        use std::os::unix::process::ExitStatusExt;

        let outcome = decide_child_outcome(
            Some(ImportCompletedEvent {
                version: 1,
                event: "import_completed".into(),
                sessions_processed: 1,
                threads_created: 1,
                memories_imported: 3,
                memories_skipped_duplicate: 0,
                memories_skipped_ignored: 0,
                errors_count: 0,
                success: true,
                sessions: Vec::new(),
            }),
            Ok(std::process::ExitStatus::from_raw(0)),
            None,
            None,
            &["codex summary:".into(), "  Memories imported: 3".into()],
            &[],
        );

        match outcome {
            ChildOutcome::Success { summary } => assert_eq!(
                summary.as_deref(),
                Some("codex summary:\n  Memories imported: 3")
            ),
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn failed_completion_event_without_completed_sessions_is_a_failure() {
        use std::os::unix::process::ExitStatusExt;

        let outcome = decide_child_outcome(
            Some(ImportCompletedEvent {
                version: 1,
                event: "import_completed".into(),
                sessions_processed: 1,
                threads_created: 0,
                memories_imported: 0,
                memories_skipped_duplicate: 0,
                memories_skipped_ignored: 0,
                errors_count: 1,
                success: false,
                sessions: Vec::new(),
            }),
            Ok(std::process::ExitStatus::from_raw(1 << 8)),
            None,
            None,
            &[],
            &[],
        );

        assert!(matches!(
            outcome,
            ChildOutcome::Failure(message)
                if message.contains("memories-import failed")
                    && message.contains("errors_count=1")
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_importer_events_drive_pending_target_and_clear_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let pending = Arc::new(parking_lot::Mutex::new(PendingImportState::default()));
        let completions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let scope = local_scope(1);
        let aggregator = Arc::new(ImportAggregator::new(1));
        let script = concat!(
            "printf '%s\\n' '",
            "{\"schema\":\"memories-import-event\",\"version\":1,\"event\":\"thread_created\",\"source\":\"claude-code\",\"session_key\":\"claude_code:session\",\"thread_id\":\"9007199254740993\"}",
            "' '",
            "{\"schema\":\"memories-import-event\",\"version\":1,\"event\":\"session_completed\",\"source\":\"claude-code\",\"session_key\":\"claude_code:session\",\"thread_id\":\"9007199254740993\",\"imported_count\":3,\"success\":true}",
            "' '",
            "{\"schema\":\"memories-import-event\",\"version\":1,\"event\":\"import_completed\",\"sessions_processed\":1,\"threads_created\":1,\"memories_imported\":3,\"memories_skipped_duplicate\":0,\"memories_skipped_ignored\":0,\"errors_count\":0,\"success\":true,\"sessions\":[{\"session_key\":\"claude_code:session\",\"status\":\"completed\",\"imported_count\":3}]}",
            "' 'claude-code summary:' '  Memories imported: 3'"
        );
        let state = run_scripted_import(
            script,
            aggregator,
            pending_path.clone(),
            pending.clone(),
            completions.clone(),
            scope.clone(),
            false,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(matches!(state, Some(AggregatedState::AllSucceeded { .. })));
        let targeted = select_targeted_sessions(
            &pending.lock(),
            &scope,
            &completions.lock(),
            &[ImportSource::ClaudeCode],
        );
        assert_eq!(targeted.len(), 1);
        assert_eq!(targeted[0].thread_id, "9007199254740993");
        assert_eq!(
            load_pending_imports(&pending_path).unwrap().sessions,
            targeted
        );

        let cleared = completed_pending_state(&pending.lock(), &targeted, true);
        save_pending_imports(&pending_path, &cleared).unwrap();
        assert!(
            load_pending_imports(&pending_path)
                .unwrap()
                .sessions
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_event_authorizes_partial_success_after_nonzero_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let pending = Arc::new(parking_lot::Mutex::new(PendingImportState::default()));
        let completions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let aggregator = Arc::new(ImportAggregator::new(1));
        let script = concat!(
            "printf '%s\\n' '",
            "{\"schema\":\"memories-import-event\",\"version\":1,\"event\":\"import_started\",\"source\":\"claude-code\",\"session_key\":\"protocol-error\",\"thread_id\":\"43\"}",
            "' '",
            "{\"schema\":\"memories-import-event\",\"version\":1,\"event\":\"thread_created\",\"source\":\"claude-code\",\"session_key\":\"completed-session\",\"thread_id\":\"42\"}",
            "' '",
            "{\"schema\":\"memories-import-event\",\"version\":1,\"event\":\"session_completed\",\"source\":\"claude-code\",\"session_key\":\"completed-session\",\"thread_id\":\"42\",\"imported_count\":1,\"success\":true}",
            "' '",
            "{\"schema\":\"memories-import-event\",\"version\":1,\"event\":\"import_completed\",\"sessions_processed\":2,\"threads_created\":1,\"memories_imported\":1,\"memories_skipped_duplicate\":0,\"memories_skipped_ignored\":0,\"errors_count\":1,\"success\":false,\"sessions\":[{\"session_key\":\"completed-session\",\"status\":\"completed\",\"imported_count\":1},{\"session_key\":\"failed-session\",\"status\":\"failed\",\"imported_count\":0,\"error\":{\"message\":\"bad input\"}}]}' ; exit 1"
        );

        let state = run_scripted_import(
            script,
            aggregator,
            pending_path,
            pending,
            completions,
            local_scope(1),
            false,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        match state {
            Some(AggregatedState::PartialSucceeded { warnings, .. }) => {
                assert_eq!(warnings.len(), 1);
                assert!(warnings[0].contains("errors_count=1"));
                assert!(warnings[0].contains("failed-session"));
                assert!(warnings[0].contains("bad input"));
                assert_eq!(
                    warnings[0]
                        .matches("unsupported memories-import event import_started@1")
                        .count(),
                    1
                );
            }
            other => panic!("expected partial success, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_completion_with_omitted_session_event_is_partial_success() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let aggregator = Arc::new(ImportAggregator::new(1));
        let event = concat!(
            "{\"schema\":\"memories-import-event\",\"version\":1,",
            "\"event\":\"thread_created\",\"source\":\"claude-code\",",
            "\"session_key\":\"missing-completion\",\"thread_id\":\"42\"}"
        );
        let completed = concat!(
            "{\"schema\":\"memories-import-event\",\"version\":1,",
            "\"event\":\"import_completed\",\"sessions_processed\":1,",
            "\"threads_created\":1,\"memories_imported\":1,",
            "\"memories_skipped_duplicate\":0,\"memories_skipped_ignored\":0,",
            "\"errors_count\":0,\"success\":true,\"sessions\":[",
            "{\"session_key\":\"missing-completion\",\"status\":\"completed\",",
            "\"imported_count\":1}]}"
        );
        let (warnings, subscriber) = warning_capture();
        let _default = tracing::subscriber::set_default(subscriber);

        let state = run_scripted_import(
            &format!("printf '%s\\n' '{event}' '{completed}'; exit 0"),
            aggregator,
            pending_path,
            Arc::new(parking_lot::Mutex::new(PendingImportState::default())),
            Arc::new(parking_lot::Mutex::new(Vec::new())),
            local_scope(1),
            false,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        match state {
            Some(AggregatedState::PartialSucceeded { warnings, .. }) => {
                assert_eq!(warnings.len(), 1);
                assert!(warnings[0].contains(
                    "memories-import omitted session_completed for claude-code/missing-completion"
                ));
            }
            other => panic!("expected partial success, got {other:?}"),
        }
        assert!(warnings.lock().iter().any(|warning| {
            warning.contains(
                "memories-import omitted session_completed for claude-code/missing-completion",
            )
        }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completion_with_raw_event_error_warns_without_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let aggregator = Arc::new(ImportAggregator::new(1));
        let malformed =
            r#"{"schema":"memories-import-event","version":1,"event":"session_completed""#;
        let completed = concat!(
            "{\"schema\":\"memories-import-event\",\"version\":1,",
            "\"event\":\"import_completed\",\"sessions_processed\":0,",
            "\"threads_created\":0,\"memories_imported\":0,",
            "\"memories_skipped_duplicate\":0,\"memories_skipped_ignored\":0,",
            "\"errors_count\":0,\"success\":true,\"sessions\":[]}"
        );
        let (warnings, subscriber) = warning_capture();
        let _default = tracing::subscriber::set_default(subscriber);

        let state = run_scripted_import(
            &format!("printf '%s\\n' '{malformed}' '{completed}'; exit 0"),
            aggregator,
            tmp.path().join("import-pending.json"),
            Arc::new(parking_lot::Mutex::new(PendingImportState::default())),
            Arc::new(parking_lot::Mutex::new(Vec::new())),
            local_scope(1),
            false,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert!(matches!(
            state,
            Some(AggregatedState::PartialSucceeded { warnings, .. })
                if warnings.iter().any(|warning| warning.contains("malformed memories-import event"))
        ));
        assert!(
            warnings
                .lock()
                .iter()
                .any(|warning| warning.contains("malformed memories-import event"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_without_completion_event_remains_a_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let aggregator = Arc::new(ImportAggregator::new(1));

        let state = run_scripted_import(
            "printf '%s\\n' 'connection failed' >&2; exit 1",
            aggregator,
            tmp.path().join("import-pending.json"),
            Arc::new(parking_lot::Mutex::new(PendingImportState::default())),
            Arc::new(parking_lot::Mutex::new(Vec::new())),
            local_scope(1),
            false,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert!(matches!(
            state,
            Some(AggregatedState::AnyFailed(errors))
                if errors.iter().any(|error| error.contains("exit"))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_completion_event_uses_unknown_session_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let aggregator = Arc::new(ImportAggregator::new(1));
        let event = concat!(
            "{\"schema\":\"memories-import-event\",\"version\":1,",
            "\"event\":\"import_completed\",\"sessions_processed\":1,",
            "\"threads_created\":0,\"memories_imported\":0,",
            "\"memories_skipped_duplicate\":0,\"memories_skipped_ignored\":0,",
            "\"errors_count\":1,\"success\":false,\"sessions\":[",
            "{\"session_key\":null,\"status\":\"failed\",",
            "\"imported_count\":0,\"error\":{\"message\":\"parse failed\"}}]}"
        );

        let state = run_scripted_import(
            &format!("printf '%s\\n' '{event}'; exit 1"),
            aggregator,
            tmp.path().join("import-pending.json"),
            Arc::new(parking_lot::Mutex::new(PendingImportState::default())),
            Arc::new(parking_lot::Mutex::new(Vec::new())),
            local_scope(1),
            false,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert!(matches!(
            state,
            Some(AggregatedState::AnyFailed(errors))
                if errors.iter().any(|error| error.contains("<unknown session>") && error.contains("parse failed"))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dry_run_summary_without_lifecycle_events_succeeds_without_state_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let pending = Arc::new(parking_lot::Mutex::new(PendingImportState::default()));
        let completions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let aggregator = Arc::new(ImportAggregator::new(1));

        let state = run_scripted_import(
            "printf '%s\\n' '[dry-run] claude-code summary:' '  Memories imported: 3'",
            aggregator,
            pending_path.clone(),
            pending.clone(),
            completions.clone(),
            local_scope(1),
            true,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert!(matches!(state, Some(AggregatedState::AllSucceeded { .. })));
        assert!(pending.lock().sessions.is_empty());
        assert!(completions.lock().is_empty());
        assert!(!pending_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_import_summary_without_lifecycle_events_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let pending = Arc::new(parking_lot::Mutex::new(PendingImportState::default()));
        let completions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let aggregator = Arc::new(ImportAggregator::new(1));

        let state = run_scripted_import(
            "printf '%s\\n' 'claude-code summary:' '  Memories imported: 3'",
            aggregator,
            pending_path,
            pending,
            completions,
            local_scope(1),
            false,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert!(matches!(
            state,
            Some(AggregatedState::AnyFailed(errors))
                if errors == ["memories-import wrote memories without versioned session events"]
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dry_run_lifecycle_event_fails_before_pending_state_is_mutated() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let pending = Arc::new(parking_lot::Mutex::new(PendingImportState::default()));
        let completions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let aggregator = Arc::new(ImportAggregator::new(1));
        let event = concat!(
            "{\"schema\":\"memories-import-event\",\"version\":1,",
            "\"event\":\"thread_created\",\"source\":\"claude-code\",",
            "\"session_key\":\"claude_code:session\",",
            "\"thread_id\":\"9007199254740993\"}"
        );

        let state = run_scripted_import(
            &format!("printf '%s\\n' '{event}'"),
            aggregator,
            pending_path.clone(),
            pending.clone(),
            completions.clone(),
            local_scope(1),
            true,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert!(matches!(
            state,
            Some(AggregatedState::AnyFailed(errors))
                if errors == ["memories-import emitted lifecycle event during dry-run"]
        ));
        assert!(pending.lock().sessions.is_empty());
        assert!(completions.lock().is_empty());
        assert!(!pending_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mismatched_lifecycle_event_source_fails_before_pending_state_is_mutated() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let pending = Arc::new(parking_lot::Mutex::new(PendingImportState::default()));
        let completions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let aggregator = Arc::new(ImportAggregator::new(1));
        let event = concat!(
            "{\"schema\":\"memories-import-event\",\"version\":1,",
            "\"event\":\"thread_created\",\"source\":\"codex\",",
            "\"session_key\":\"codex:session\",\"thread_id\":\"42\"}"
        );

        let state = run_scripted_import(
            &format!("printf '%s\\n' '{event}'"),
            aggregator,
            pending_path.clone(),
            pending.clone(),
            completions.clone(),
            local_scope(1),
            false,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert!(
            matches!(
                state,
                Some(AggregatedState::AnyFailed(ref errors))
                    if errors.as_slice() == ["config: memories-import lifecycle event source mismatch: expected claude-code, got codex"]
            ),
            "unexpected importer state: {state:?}"
        );
        assert!(pending.lock().sessions.is_empty());
        assert!(completions.lock().is_empty());
        assert!(!pending_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_joins_output_readers_before_the_next_import_updates_pending_state() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let pending = Arc::new(parking_lot::Mutex::new(PendingImportState::default()));
        let completions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let aggregator = Arc::new(ImportAggregator::new(1));
        let old_event = concat!(
            "{\"schema\":\"memories-import-event\",\"version\":1,",
            "\"event\":\"thread_created\",\"source\":\"claude-code\",",
            "\"session_key\":\"old-session\",\"thread_id\":\"42\"}"
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        let task_cancel = cancel.clone();
        let task_pending_path = pending_path.clone();
        let task_pending = pending.clone();
        let script = format!("printf '%s\\n' '{old_event}'; sleep 5");
        let task = tokio::spawn(async move {
            run_scripted_import(
                &script,
                aggregator,
                task_pending_path,
                task_pending,
                completions,
                local_scope(1),
                false,
                task_cancel,
            )
            .await
        });

        cancel.cancel();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("cancellation must wait for reader shutdown")
            .unwrap();
        assert_eq!(outcome, None);

        let new_session =
            pending_session(&local_scope(1), ImportSource::Codex, "new-session", "43");
        record_pending_import(&pending_path, &mut pending.lock(), new_session.clone()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            load_pending_imports(&pending_path).unwrap().sessions,
            vec![new_session]
        );
    }

    #[test]
    fn failure_message_appends_stderr_tail() {
        let stderr = [
            "Error: connect to https://memories.example.com:9000".to_string(),
            "  Caused by: invalid peer certificate: UnknownIssuer".to_string(),
        ];
        let msg = failure_message("exit status: 1", &stderr);
        assert_eq!(
            msg,
            "exit status: 1\nError: connect to https://memories.example.com:9000\n  Caused by: invalid peer certificate: UnknownIssuer"
        );
    }

    #[test]
    fn failure_message_keeps_only_the_last_lines_in_order() {
        // More than STDERR_TAIL_LINES: keep the final 5, oldest-first.
        let stderr: Vec<String> = (1..=8).map(|n| format!("line {n}")).collect();
        let msg = failure_message("exit status: 1", &stderr);
        assert_eq!(
            msg,
            "exit status: 1\nline 4\nline 5\nline 6\nline 7\nline 8"
        );
    }

    #[test]
    fn failure_message_is_reason_only_when_stderr_empty() {
        // No stderr (or only blank lines) → just the exit reason, no trailing
        // newline that would render as an empty line in the toast.
        assert_eq!(failure_message("exit status: 1", &[]), "exit status: 1");
        let blanks = ["".to_string(), "   ".to_string()];
        assert_eq!(failure_message("wait: x", &blanks), "wait: x");
    }
}
