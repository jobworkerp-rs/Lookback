use std::collections::BTreeMap;
use std::path::PathBuf;

use super::super::connection::ConnectionMode;
use super::guards::ImportSource;
use super::pending::{PendingImportScope, PendingImportSession};
use super::request::{PlainImportConfig, StartImportRequest, ThreadStrategy};

pub(super) fn local_scope(user_id: i64) -> PendingImportScope {
    PendingImportScope::new(ConnectionMode::Local, "http://127.0.0.1:9010", user_id).unwrap()
}

pub(super) fn pending_session(
    scope: &PendingImportScope,
    source: ImportSource,
    session_key: &str,
    thread_id: &str,
) -> PendingImportSession {
    PendingImportSession {
        memories_endpoint_id: scope.memories_endpoint_id.clone(),
        user_id: scope.user_id,
        source,
        session_key: session_key.into(),
        thread_id: thread_id.into(),
        lifecycle_id: None,
        plan: None,
        steps: BTreeMap::new(),
    }
}

pub(super) fn plain_cfg(source_name: Option<&str>, strategy: ThreadStrategy) -> PlainImportConfig {
    PlainImportConfig {
        root: PathBuf::from("/tmp/notes"),
        source_name: source_name.map(str::to_string),
        thread_strategy: strategy,
    }
}

pub(super) fn make_request(
    run_summary: bool,
    run_personality: bool,
    run_reflection: bool,
) -> StartImportRequest {
    StartImportRequest {
        sources: vec![ImportSource::ClaudeCode],
        since: None,
        user_id: Some(1),
        dry_run: false,
        labels: vec![],
        memories_import_bin: PathBuf::from("/usr/local/bin/memories-import"),
        run_summary,
        run_personality,
        run_reflection,
        dispatch_id: None,
        plain: None,
    }
}

pub(super) fn plain_request(plain: Option<PlainImportConfig>) -> StartImportRequest {
    StartImportRequest {
        sources: vec![ImportSource::Plain],
        since: None,
        user_id: Some(1),
        dry_run: false,
        labels: vec![],
        memories_import_bin: PathBuf::from("/usr/local/bin/memories-import"),
        run_summary: false,
        run_personality: false,
        run_reflection: false,
        dispatch_id: None,
        plain,
    }
}
