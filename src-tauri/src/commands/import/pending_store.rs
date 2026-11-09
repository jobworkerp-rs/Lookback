use std::collections::BTreeSet;
use std::path::Path;

use super::super::{AppError, AppResult};
use super::aggregator::step_label;
use super::events::ImportSessionCompletedEvent;
use super::guards::ImportSource;
use super::pending::{
    PENDING_IMPORT_FORMAT_VERSION, PendingImportScope, PendingImportSession, PendingImportState,
    PendingImportStep, canonical_json,
};
use super::request::ImportStep;

pub(super) fn load_pending_imports(path: &Path) -> AppResult<PendingImportState> {
    match std::fs::read(path) {
        Ok(raw) => {
            let value: serde_json::Value = serde_json::from_slice(&raw)
                .map_err(|error| AppError::Config(format!("read import pending state: {error}")))?;
            let format_version = value
                .get("format_version")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    AppError::Config("import pending state format_version is missing".into())
                })?;
            if format_version == 1 {
                let sessions = value
                    .get("sessions")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| {
                        AppError::Config("legacy import pending state sessions is invalid".into())
                    })?;
                if sessions.is_empty() {
                    return Ok(PendingImportState::default());
                }
                return Err(AppError::Config(format!(
                    "import pending state format_version=1 contains sessions without a Memories endpoint or user scope; inspect and move aside {} before importing",
                    path.display()
                )));
            }
            if format_version == 2 {
                let mut migrated: PendingImportState =
                    serde_json::from_value(value).map_err(|error| {
                        AppError::Config(format!("read legacy import pending state: {error}"))
                    })?;
                migrated.format_version = PENDING_IMPORT_FORMAT_VERSION;
                save_pending_imports(path, &migrated)?;
                return Ok(migrated);
            }
            if format_version != u64::from(PENDING_IMPORT_FORMAT_VERSION) {
                return Err(AppError::Config(format!(
                    "unsupported import pending state format_version: {format_version}"
                )));
            }
            serde_json::from_value(value)
                .map_err(|error| AppError::Config(format!("read import pending state: {error}")))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(PendingImportState::default())
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) fn save_pending_imports(path: &Path, state: &PendingImportState) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("import pending path has no parent".into()))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".import-pending-{}.tmp", std::process::id()));
    let raw = serde_json::to_vec_pretty(state)
        .map_err(|error| AppError::Config(format!("serialize import pending state: {error}")))?;
    let mut file = std::fs::File::create(&tmp)?;
    use std::io::Write;
    file.write_all(&raw)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub(super) fn record_pending_import(
    path: &Path,
    state: &mut PendingImportState,
    session: PendingImportSession,
) -> AppResult<()> {
    if let Some(existing) = state
        .sessions
        .iter_mut()
        .find(|existing| existing.identity() == session.identity())
    {
        if existing.thread_id != session.thread_id {
            return Err(AppError::Config(format!(
                "memories-import changed thread_id for stable session key {}",
                session.session_key
            )));
        }
    } else {
        state.sessions.push(session);
    }
    state.sessions.sort_by(|left, right| {
        (
            &left.memories_endpoint_id,
            left.user_id,
            &left.source,
            &left.session_key,
        )
            .cmp(&(
                &right.memories_endpoint_id,
                right.user_id,
                &right.source,
                &right.session_key,
            ))
    });
    save_pending_imports(path, state)
}

// The call sites hold every value as a local already; bundling them into a
// struct would duplicate the field list without narrowing any invariant.
#[allow(clippy::too_many_arguments)]
pub(super) fn record_pending_operation(
    path: &Path,
    state: &mut PendingImportState,
    targeted: &[PendingImportSession],
    step: ImportStep,
    operation_key: &str,
    plan: &serde_json::Value,
    job_id: Option<&str>,
    succeeded: bool,
) -> AppResult<()> {
    // Stamp by canonical identity, never by bare `thread_id`: the pending
    // file accumulates sessions across endpoints / users / sources and a
    // leftover local-mode session can reuse the numeric thread id of the
    // remote session this run targets, which would otherwise inherit this
    // run's plan, operation key and `retained` flag.
    let targets = targeted
        .iter()
        .map(PendingImportSession::identity)
        .collect::<BTreeSet<_>>();
    for session in &mut state.sessions {
        if targets.contains(&session.identity()) {
            session.plan = Some(canonical_json(plan));
            // A pre-dispatch retry carries no job id yet; keep the previous
            // attempt's id when the key matches so an orphaned running job
            // stays referenceable until a fresh dispatch replaces it.
            let preserved_job_id = job_id.map(str::to_owned).or_else(|| {
                session
                    .steps
                    .get(step_label(step))
                    .filter(|stored| stored.operation_key == operation_key)
                    .and_then(|stored| stored.job_id.clone())
            });
            session.steps.insert(
                step_label(step).to_owned(),
                PendingImportStep {
                    operation_key: operation_key.to_owned(),
                    job_id: preserved_job_id,
                    succeeded,
                    retained: !succeeded,
                },
            );
        }
    }
    save_pending_imports(path, state)
}

pub(super) fn completed_pending_state(
    current: &PendingImportState,
    targeted: &[PendingImportSession],
    downstream_completed: bool,
) -> PendingImportState {
    if !downstream_completed {
        return current.clone();
    }
    // Identity, not whole-struct equality: `targeted` was selected before the
    // dispatch loop, while the stored copies have since been stamped with
    // `plan` / `steps` by `record_pending_operation`. Comparing full structs
    // would never match again and the completed sessions would accumulate in
    // `import-pending.json` forever.
    let cleared = targeted
        .iter()
        .map(PendingImportSession::identity)
        .collect::<BTreeSet<_>>();
    let mut next = current.clone();
    next.sessions
        .retain(|session| !cleared.contains(&session.identity()));
    next
}

pub(super) fn select_targeted_sessions(
    pending: &PendingImportState,
    scope: &PendingImportScope,
    completions: &[ImportSessionCompletedEvent],
    selected_sources: &[ImportSource],
) -> Vec<PendingImportSession> {
    pending
        .sessions
        .iter()
        .filter(|session| {
            scope.contains(session)
                && selected_sources.contains(&session.source)
                && completions.iter().any(|completed| {
                    completed.success
                        && completed.source == session.source
                        && completed.session_key == session.session_key
                        && completed.thread_id == session.thread_id
                })
        })
        .cloned()
        .collect()
}

pub(super) fn should_dispatch_import_targets(target_thread_ids: &[String]) -> bool {
    !target_thread_ids.is_empty()
}

#[cfg(test)]
mod tests {
    use super::super::super::connection::ConnectionMode;
    use super::super::events::{
        CompletionDisposition, ImportSessionCompletedEvent, classify_session_completion,
    };
    use super::super::guards::ImportSource;
    use super::super::pending::{
        PENDING_IMPORT_FORMAT_VERSION, PendingImportScope, PendingImportSession, PendingImportState,
    };
    use super::super::request::ImportStep;
    use super::super::tests_common::{local_scope, pending_session};
    use super::*;

    #[test]
    fn pending_success_completion_targets_retry_regardless_of_imported_count() {
        let scope = local_scope(1);
        let session = pending_session(&scope, ImportSource::Codex, "codex:partial", "42");
        let pending = PendingImportState {
            format_version: PENDING_IMPORT_FORMAT_VERSION,
            sessions: vec![session.clone()],
        };
        let zero = ImportSessionCompletedEvent {
            source: session.source,
            session_key: session.session_key.clone(),
            thread_id: session.thread_id.clone(),
            imported_count: 0,
            success: true,
        };

        assert_eq!(
            classify_session_completion(&pending, &scope, &zero).unwrap(),
            CompletionDisposition::TrackPending
        );
        assert_eq!(
            select_targeted_sessions(
                &pending,
                &scope,
                std::slice::from_ref(&zero),
                &[ImportSource::Codex],
            ),
            vec![session.clone()]
        );

        let failed = ImportSessionCompletedEvent {
            success: false,
            ..zero.clone()
        };
        assert!(
            select_targeted_sessions(&pending, &scope, &[failed], &[ImportSource::Codex])
                .is_empty()
        );

        let wrong_source = ImportSessionCompletedEvent {
            source: ImportSource::ClaudeCode,
            ..zero.clone()
        };
        assert!(
            select_targeted_sessions(
                &pending,
                &scope,
                &[wrong_source],
                &[ImportSource::Codex, ImportSource::ClaudeCode],
            )
            .is_empty()
        );

        assert!(
            select_targeted_sessions(&pending, &local_scope(2), &[zero], &[ImportSource::Codex],)
                .is_empty()
        );
    }

    #[test]
    fn pending_state_is_atomic_deduplicated_and_rejects_id_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        let scope = local_scope(1);
        let session = pending_session(&scope, ImportSource::Codex, "stable", "9007199254740993");
        let mut state = PendingImportState::default();
        record_pending_import(&path, &mut state, session.clone()).unwrap();
        record_pending_import(&path, &mut state, session.clone()).unwrap();
        assert_eq!(
            load_pending_imports(&path).unwrap().sessions,
            vec![session.clone()]
        );
        assert!(
            record_pending_import(
                &path,
                &mut state,
                PendingImportSession {
                    thread_id: "43".into(),
                    ..session
                }
            )
            .is_err()
        );
    }

    #[test]
    fn pending_operation_is_written_before_dispatch_and_retained_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        let scope = PendingImportScope {
            memories_endpoint_id: "local".into(),
            user_id: 1,
        };
        let session = pending_session(&scope, ImportSource::OpenCode, "opencode:ses-1", "42");
        let mut state = PendingImportState {
            format_version: PENDING_IMPORT_FORMAT_VERSION,
            sessions: vec![session.clone()],
        };
        let plan = serde_json::json!({"thread_ids": ["42"]});

        record_pending_operation(
            &path,
            &mut state,
            std::slice::from_ref(&session),
            ImportStep::ThreadSummary,
            "lookback-op-test",
            &plan,
            None,
            false,
        )
        .unwrap();
        let stored = &state.sessions[0].steps["thread-summary"];
        assert_eq!(stored.operation_key, "lookback-op-test");
        assert_eq!(stored.job_id, None);
        assert!(stored.retained);

        record_pending_operation(
            &path,
            &mut state,
            std::slice::from_ref(&session),
            ImportStep::ThreadSummary,
            "lookback-op-test",
            &plan,
            Some("123"),
            true,
        )
        .unwrap();
        assert!(state.sessions[0].steps["thread-summary"].succeeded);
        assert_eq!(
            state.sessions[0].steps["thread-summary"].job_id.as_deref(),
            Some("123")
        );
        assert!(!state.sessions[0].steps["thread-summary"].retained);

        // A pre-dispatch retry of the same key keeps the orphan's job id;
        // a different key starts a fresh record without the stale id.
        record_pending_operation(
            &path,
            &mut state,
            std::slice::from_ref(&session),
            ImportStep::ThreadSummary,
            "lookback-op-test",
            &plan,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            state.sessions[0].steps["thread-summary"].job_id.as_deref(),
            Some("123")
        );
        record_pending_operation(
            &path,
            &mut state,
            std::slice::from_ref(&session),
            ImportStep::ThreadSummary,
            "lookback-op-new",
            &plan,
            None,
            false,
        )
        .unwrap();
        assert_eq!(state.sessions[0].steps["thread-summary"].job_id, None);
    }

    #[test]
    fn pending_operation_uses_a_distinct_thread_group_summary_step_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        let scope = local_scope(1);
        let session = pending_session(&scope, ImportSource::Codex, "codex:group", "42");
        let mut state = PendingImportState {
            format_version: PENDING_IMPORT_FORMAT_VERSION,
            sessions: vec![session.clone()],
        };

        record_pending_operation(
            &path,
            &mut state,
            std::slice::from_ref(&session),
            ImportStep::ThreadGroupSummary,
            "lookback-group-test",
            &serde_json::json!({"trigger_kind": "import"}),
            None,
            false,
        )
        .unwrap();

        assert_eq!(
            state.sessions[0].steps["thread-group-summary"].operation_key,
            "lookback-group-test"
        );
        assert!(state.sessions[0].steps["thread-group-summary"].retained);
        assert!(!state.sessions[0].steps.contains_key("thread-summary"));
    }

    #[test]
    fn pending_operation_stamps_only_the_canonically_targeted_session() {
        // `import-pending.json` is one global file, so a session that merely
        // reuses the target's thread id — on another endpoint, for another
        // user, or from another source — must not inherit this run's plan,
        // operation key or retention flag.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        let local = PendingImportScope {
            memories_endpoint_id: "local".into(),
            user_id: 1,
        };
        let remote =
            PendingImportScope::new(ConnectionMode::Remote, "https://remote.example:443", 1)
                .unwrap();
        let targeted = pending_session(&local, ImportSource::OpenCode, "opencode:ses-1", "42");
        let other_endpoint =
            pending_session(&remote, ImportSource::OpenCode, "opencode:ses-1", "42");
        let other_user = PendingImportSession {
            user_id: 7,
            ..pending_session(&local, ImportSource::OpenCode, "opencode:ses-1", "42")
        };
        let other_source = pending_session(&local, ImportSource::Codex, "codex:ses-1", "42");
        let mut state = PendingImportState {
            format_version: PENDING_IMPORT_FORMAT_VERSION,
            sessions: vec![
                other_endpoint.clone(),
                targeted.clone(),
                other_user.clone(),
                other_source.clone(),
            ],
        };

        record_pending_operation(
            &path,
            &mut state,
            std::slice::from_ref(&targeted),
            ImportStep::ThreadSummary,
            "op-target",
            &serde_json::json!({"thread_ids": ["42"]}),
            Some("7"),
            true,
        )
        .unwrap();

        let reloaded = load_pending_imports(&path).unwrap();
        assert_eq!(
            reloaded.sessions[1].steps["thread-summary"].operation_key,
            "op-target"
        );
        assert!(reloaded.sessions[1].steps["thread-summary"].succeeded);
        assert_eq!(
            reloaded.sessions[1].steps["thread-summary"]
                .job_id
                .as_deref(),
            Some("7")
        );
        assert!(reloaded.sessions[1].plan.is_some());
        for bystander in [0, 2, 3] {
            assert!(
                reloaded.sessions[bystander].steps.is_empty(),
                "session {bystander} shares only a thread id with the target"
            );
            assert!(reloaded.sessions[bystander].plan.is_none());
        }
    }

    #[test]
    fn remote_databases_keep_same_session_with_different_thread_ids_separate() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        let mut state = PendingImportState::default();
        let remote_a =
            PendingImportScope::new(ConnectionMode::Remote, "https://remote-a.example:443", 7)
                .unwrap();
        let remote_b =
            PendingImportScope::new(ConnectionMode::Remote, "https://remote-b.example:443", 7)
                .unwrap();
        record_pending_import(
            &path,
            &mut state,
            pending_session(&remote_a, ImportSource::Codex, "codex:same-session", "42"),
        )
        .unwrap();

        let recorded_b = record_pending_import(
            &path,
            &mut state,
            pending_session(&remote_b, ImportSource::Codex, "codex:same-session", "43"),
        );

        assert!(
            recorded_b.is_ok(),
            "a different Memories DB is a separate key"
        );
        assert_eq!(state.sessions.len(), 2);
        let completion_b = ImportSessionCompletedEvent {
            source: ImportSource::Codex,
            session_key: "codex:same-session".into(),
            thread_id: "43".into(),
            imported_count: 1,
            success: true,
        };
        assert_eq!(
            classify_session_completion(&state, &remote_b, &completion_b).unwrap(),
            CompletionDisposition::TrackPending
        );
        let targeted_b =
            select_targeted_sessions(&state, &remote_b, &[completion_b], &[ImportSource::Codex]);
        assert_eq!(
            targeted_b,
            vec![pending_session(
                &remote_b,
                ImportSource::Codex,
                "codex:same-session",
                "43",
            )]
        );
        let after_b_completed = completed_pending_state(&state, &targeted_b, true);
        assert_eq!(after_b_completed.sessions.len(), 1);
        assert!(
            after_b_completed
                .sessions
                .iter()
                .any(|session| remote_a.contains(session))
        );
    }

    #[test]
    fn pending_state_loader_upgrades_empty_format_version_one() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        std::fs::write(&path, r#"{"format_version":1,"sessions":[]}"#).unwrap();

        let state = load_pending_imports(&path).unwrap();
        assert_eq!(state, PendingImportState::default());
    }

    #[test]
    fn pending_state_loader_fails_closed_for_unscoped_format_version_one() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("import-pending.json");
        std::fs::write(
            &path,
            r#"{"format_version":1,"sessions":[{"source":"codex","session_key":"stable","thread_id":"42"}]}"#,
        )
        .unwrap();

        let error = load_pending_imports(&path).unwrap_err().to_string();
        assert!(error.contains("without a Memories endpoint or user scope"));
        assert!(error.contains("inspect and move aside"));
        assert!(error.contains("import-pending.json"));
    }

    #[test]
    fn pending_state_loader_migrates_format_version_two_with_empty_operation_state() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        std::fs::write(
            &path,
            r#"{"format_version":2,"sessions":[{"memories_endpoint_id":"local","user_id":1,"source":"opencode","session_key":"opencode:ses-1","thread_id":"42"}]}"#,
        )
        .unwrap();

        let state = load_pending_imports(&path).unwrap();
        assert_eq!(state.format_version, PENDING_IMPORT_FORMAT_VERSION);
        assert_eq!(state.sessions.len(), 1);
        assert!(state.sessions[0].plan.is_none());
        assert!(state.sessions[0].steps.is_empty());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()["format_version"],
            3
        );
    }

    #[test]
    fn pending_state_loader_rejects_unknown_format_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        std::fs::write(&path, r#"{"format_version":4,"sessions":[]}"#).unwrap();

        let error = load_pending_imports(&path).unwrap_err().to_string();
        assert!(error.contains("unsupported import pending state format_version"));
    }

    #[test]
    fn pending_state_loader_rejects_malformed_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        std::fs::write(&path, r#"{"format_version":1,"sessions":"#).unwrap();

        let error = load_pending_imports(&path).unwrap_err().to_string();
        assert!(error.contains("read import pending state"));
    }

    #[test]
    fn pending_endpoint_identity_normalizes_remote_authority_without_secrets() {
        let variants = [
            "https://EXAMPLE.com",
            "https://example.com:443/tenant/path?token=secret#fragment",
            "https://user:password@example.com/other",
        ];
        let identities = variants.map(|url| {
            PendingImportScope::new(ConnectionMode::Remote, url, 1)
                .unwrap()
                .memories_endpoint_id
        });
        assert_eq!(identities[0], identities[1]);
        assert_eq!(identities[1], identities[2]);
        assert!(identities[0].starts_with("remote-sha256:"));
        assert!(!identities[0].contains("example"));
        assert!(!identities[0].contains("user"));
        assert!(!identities[0].contains("password"));
        assert!(!identities[0].contains("secret"));
        assert_ne!(
            identities[0],
            PendingImportScope::new(ConnectionMode::Remote, "http://example.com:443", 1)
                .unwrap()
                .memories_endpoint_id
        );
    }

    #[test]
    fn pending_state_persists_only_the_remote_endpoint_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("import-pending.json");
        let scope = PendingImportScope::new(
            ConnectionMode::Remote,
            "https://alice:private-token@Secret-Host.example/path?api_key=hidden",
            1,
        )
        .unwrap();
        let mut state = PendingImportState::default();
        record_pending_import(
            &path,
            &mut state,
            pending_session(&scope, ImportSource::Codex, "session", "42"),
        )
        .unwrap();

        let persisted = std::fs::read_to_string(path).unwrap();
        assert!(persisted.contains("remote-sha256:"));
        for secret in ["alice", "private-token", "Secret-Host", "api_key", "hidden"] {
            assert!(!persisted.contains(secret));
        }
    }

    #[test]
    fn pending_endpoint_identity_normalizes_ipv6_and_default_port() {
        let compact =
            PendingImportScope::new(ConnectionMode::Remote, "https://[2001:db8::1]/path", 1)
                .unwrap();
        let expanded = PendingImportScope::new(
            ConnectionMode::Remote,
            "https://[2001:0db8:0:0:0:0:0:1]:443",
            1,
        )
        .unwrap();
        assert_eq!(compact.memories_endpoint_id, expanded.memories_endpoint_id);
        assert_ne!(
            compact.memories_endpoint_id,
            PendingImportScope::new(ConnectionMode::Remote, "https://[2001:db8::1]:444", 1)
                .unwrap()
                .memories_endpoint_id
        );
    }

    #[test]
    fn local_pending_endpoint_identity_ignores_dynamic_sidecar_port() {
        assert_eq!(
            PendingImportScope::new(ConnectionMode::Local, "http://127.0.0.1:49152", 1)
                .unwrap()
                .memories_endpoint_id,
            PendingImportScope::new(ConnectionMode::Local, "http://127.0.0.1:61000", 1)
                .unwrap()
                .memories_endpoint_id
        );
    }

    #[test]
    fn same_remote_database_separates_pending_sessions_by_user() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        let user_a =
            PendingImportScope::new(ConnectionMode::Remote, "https://example.com", 1).unwrap();
        let user_b =
            PendingImportScope::new(ConnectionMode::Remote, "https://example.com:443/path", 2)
                .unwrap();
        let mut state = PendingImportState::default();
        record_pending_import(
            &path,
            &mut state,
            pending_session(&user_a, ImportSource::Codex, "same", "42"),
        )
        .unwrap();
        record_pending_import(
            &path,
            &mut state,
            pending_session(&user_b, ImportSource::Codex, "same", "43"),
        )
        .unwrap();
        assert_eq!(state.sessions.len(), 2);
    }

    #[test]
    fn completed_pending_state_clears_sessions_already_stamped_by_the_run() {
        // The driver snapshots `targeted_sessions` before the dispatch loop
        // and stamps plan/steps onto the *stored* copies afterwards, so the
        // completion clear must key on identity — whole-struct equality would
        // no longer match and `import-pending.json` would never drain.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("import-pending.json");
        let scope = local_scope(1);
        let session = pending_session(&scope, ImportSource::OpenCode, "opencode:ses-1", "42");
        let bystander = pending_session(&scope, ImportSource::Codex, "codex:ses-2", "43");
        let mut state = PendingImportState::default();
        record_pending_import(&path, &mut state, session.clone()).unwrap();
        record_pending_import(&path, &mut state, bystander.clone()).unwrap();

        let completions = [ImportSessionCompletedEvent {
            source: ImportSource::OpenCode,
            session_key: "opencode:ses-1".into(),
            thread_id: "42".into(),
            imported_count: 3,
            success: true,
        }];
        let targeted =
            select_targeted_sessions(&state, &scope, &completions, &[ImportSource::OpenCode]);
        assert_eq!(targeted.len(), 1);

        record_pending_operation(
            &path,
            &mut state,
            &targeted,
            ImportStep::ThreadSummary,
            "op-clear",
            &serde_json::json!({"thread_ids": ["42"]}),
            Some("7"),
            true,
        )
        .unwrap();

        let cleared = completed_pending_state(&state, &targeted, true);
        assert_eq!(cleared.sessions, vec![bystander.clone()]);
        save_pending_imports(&path, &cleared).unwrap();
        assert_eq!(
            load_pending_imports(&path).unwrap().sessions,
            vec![bystander]
        );

        // A run whose downstream did not complete must keep everything.
        assert_eq!(completed_pending_state(&state, &targeted, false), state);
    }

    #[test]
    fn pending_is_kept_on_partial_failure_or_cancel_and_cleared_after_retry_success() {
        let scope = local_scope(1);
        let session = pending_session(&scope, ImportSource::Plain, "file", "42");
        let state = PendingImportState {
            format_version: PENDING_IMPORT_FORMAT_VERSION,
            sessions: vec![session.clone()],
        };
        assert_eq!(
            completed_pending_state(&state, std::slice::from_ref(&session), false),
            state
        );
        assert!(
            completed_pending_state(&state, &[session], true)
                .sessions
                .is_empty()
        );
    }

    #[test]
    fn downstream_failure_then_app_restart_targets_duplicate_retry_with_positive_count() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("import-pending.json");
        let scope = local_scope(1);
        let session = pending_session(&scope, ImportSource::Codex, "codex:partial", "42");
        let mut initial = PendingImportState::default();
        record_pending_import(&path, &mut initial, session.clone()).unwrap();

        let retained = completed_pending_state(&initial, std::slice::from_ref(&session), false);
        save_pending_imports(&path, &retained).unwrap();
        let after_restart = load_pending_imports(&path).unwrap();
        let duplicate_retry = ImportSessionCompletedEvent {
            source: ImportSource::Codex,
            session_key: "codex:partial".into(),
            thread_id: "42".into(),
            // Memories counts duplicate AddMemoriesBatch outcomes in the
            // cumulative completion count on a retry.
            imported_count: 1,
            success: true,
        };

        assert_eq!(
            select_targeted_sessions(
                &after_restart,
                &scope,
                &[duplicate_retry],
                &[ImportSource::Codex],
            ),
            vec![session]
        );
    }
}
