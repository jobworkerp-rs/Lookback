//! Profile authentication terminals share agent homes but never join an Issue run.

#[cfg(all(test, unix))]
use super::registry::native_interactive_rpc_test_fixture as rpc_fixture;

use super::{
    NativeTerminalOutputEvent, NativeTerminalOutputStore,
    wire::{NativeTerminalOutputPageWire, NativeTerminalOutputRecordWire},
};
use crate::jobworkerp::{
    JobworkerpHandle,
    coding_agent::{CodingAgentEvent, CodingAgentInput, CodingAgentStream},
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::Path, sync::Arc, time::Duration};
use thiserror::Error;

const MAX_SESSIONS: usize = 128;
const MAX_ACTIVE_SESSIONS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSettingsState {
    Starting,
    Running,
    Cancelling,
    Closed,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSettingsSession {
    pub session_id: String,
    pub profile_id: String,
    pub profile_version: u64,
    pub settings_revision: u64,
    pub image_ref: String,
    pub state: ProfileSettingsState,
    pub job_id: Option<String>,
    pub exit_code: Option<i32>,
    pub input_closed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum ProfileSettingsError {
    #[error("profile terminal request is invalid")]
    Invalid,
    #[error("profile terminal is busy or conflicts with its current state")]
    Conflict,
    #[error("profile terminal was not found")]
    NotFound,
    #[error("profile terminal belongs to another window")]
    OwnerMismatch,
    #[error("profile terminal operation is unavailable or its outcome is unknown")]
    Unavailable,
}

#[async_trait::async_trait]
#[allow(clippy::double_must_use)]
trait ProfileSettingsControl: Send + Sync {
    async fn send_input(&self, bytes: Vec<u8>) -> Result<(), ProfileSettingsError>;
    async fn finish_input(&self) -> Result<(), ProfileSettingsError>;
    async fn cancel(&self) -> Result<(), ProfileSettingsError>;
}

struct NativeControl {
    handle: JobworkerpHandle,
    job_id: jobworkerp_client::jobworkerp::data::JobId,
    input: CodingAgentInput,
}
#[async_trait::async_trait]
impl ProfileSettingsControl for NativeControl {
    async fn send_input(&self, bytes: Vec<u8>) -> Result<(), ProfileSettingsError> {
        self.input
            .send_input(bytes)
            .await
            .map_err(|_| ProfileSettingsError::Unavailable)
    }
    async fn finish_input(&self) -> Result<(), ProfileSettingsError> {
        self.input
            .finish_input()
            .await
            .map_err(|_| ProfileSettingsError::Unavailable)
    }
    async fn cancel(&self) -> Result<(), ProfileSettingsError> {
        self.handle
            .cancel(self.job_id)
            .await
            .map_err(|_| ProfileSettingsError::Unavailable)
    }
}

struct Session {
    owner: String,
    wire: ProfileSettingsSession,
    control: Option<Arc<dyn ProfileSettingsControl>>,
    cancel_requested: bool,
}

/// Small owner-bound session state; the existing bounded RAM store owns output.
pub struct ProfileSettingsTerminals {
    sessions: Mutex<Vec<Session>>,
    output: NativeTerminalOutputStore,
}

impl Default for ProfileSettingsTerminals {
    fn default() -> Self {
        Self::new()
    }
}
impl ProfileSettingsTerminals {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(Vec::new()),
            output: NativeTerminalOutputStore::open(Path::new("profile-terminal-display"))
                .expect("RAM output store"),
        }
    }

    pub(crate) fn reserve(
        &self,
        owner: &str,
        profile: &str,
        version: u64,
        revision: u64,
        image: &str,
    ) -> Result<ProfileSettingsSession, ProfileSettingsError> {
        if owner.is_empty()
            || owner.len() > 256
            || owner.chars().any(char::is_control)
            || profile.is_empty()
            || profile.len() > 256
            || version == 0
            || revision == 0
        {
            return Err(ProfileSettingsError::Invalid);
        }
        let mut sessions = self.sessions.lock();
        if sessions.iter().any(|session| {
            session.wire.profile_id == profile && session.wire.state != ProfileSettingsState::Closed
        }) || sessions
            .iter()
            .filter(|session| session.wire.state != ProfileSettingsState::Closed)
            .count()
            >= MAX_ACTIVE_SESSIONS
        {
            return Err(ProfileSettingsError::Conflict);
        }
        if sessions.len() >= MAX_SESSIONS {
            if let Some(index) = sessions
                .iter()
                .position(|session| session.wire.state == ProfileSettingsState::Closed)
            {
                sessions.remove(index);
            } else {
                return Err(ProfileSettingsError::Conflict);
            }
        }
        let wire = ProfileSettingsSession {
            session_id: super::registry::new_uuid().ok_or(ProfileSettingsError::Unavailable)?,
            profile_id: profile.into(),
            profile_version: version,
            settings_revision: revision,
            image_ref: image.into(),
            state: ProfileSettingsState::Starting,
            job_id: None,
            exit_code: None,
            input_closed: false,
        };
        sessions.push(Session {
            owner: owner.into(),
            wire: wire.clone(),
            control: None,
            cancel_requested: false,
        });
        Ok(wire)
    }

    fn accept(
        &self,
        id: &str,
        job_id: i64,
        control: Arc<dyn ProfileSettingsControl>,
    ) -> Result<(), ProfileSettingsError> {
        let mut sessions = self.sessions.lock();
        let session = sessions
            .iter_mut()
            .find(|session| session.wire.session_id == id)
            .ok_or(ProfileSettingsError::NotFound)?;
        if job_id <= 0
            || session.control.is_some()
            || session.wire.state == ProfileSettingsState::Closed
        {
            return Err(ProfileSettingsError::Conflict);
        }
        session.wire.job_id = Some(job_id.to_string());
        session.wire.state = ProfileSettingsState::Running;
        session.control = Some(control);
        Ok(())
    }

    pub(crate) fn observe(
        self: &Arc<Self>,
        id: &str,
        handle: JobworkerpHandle,
        mut stream: CodingAgentStream,
    ) -> Result<(), ProfileSettingsError> {
        let control = Arc::new(NativeControl {
            handle,
            job_id: *stream.job_id(),
            input: stream
                .input_handle()
                .ok_or(ProfileSettingsError::Unavailable)?,
        });
        self.accept(id, stream.job_id().value, control)?;
        let guard = StreamGuard {
            manager: self.clone(),
            id: id.into(),
        };
        tokio::spawn(async move {
            let guard = guard;
            loop {
                let event = match stream.next_event().await {
                    Ok(Some(CodingAgentEvent::Output { stream, data })) => {
                        NativeTerminalOutputEvent::Output { stream, data }
                    }
                    Ok(Some(CodingAgentEvent::Exit {
                        exit_code,
                        execution_time_ms,
                        sandbox_id,
                    })) => NativeTerminalOutputEvent::Exit {
                        exit_code,
                        execution_time_ms,
                        sandbox_id,
                    },
                    Ok(None) => NativeTerminalOutputEvent::End,
                    Err(_) => break,
                };
                let ended = matches!(event, NativeTerminalOutputEvent::End);
                if guard.manager.record(&guard.id, event).is_err() || ended {
                    break;
                }
            }
        });
        Ok(())
    }

    fn record(
        &self,
        id: &str,
        event: NativeTerminalOutputEvent,
    ) -> Result<(), ProfileSettingsError> {
        let mut sessions = self.sessions.lock();
        let session = sessions
            .iter_mut()
            .find(|session| session.wire.session_id == id)
            .ok_or(ProfileSettingsError::NotFound)?;
        if session.wire.state == ProfileSettingsState::Closed {
            return Err(ProfileSettingsError::Conflict);
        }
        self.output
            .append(id, event.clone())
            .map_err(|_| ProfileSettingsError::Unavailable)?;
        match event {
            NativeTerminalOutputEvent::Exit { exit_code, .. } => {
                session.wire.exit_code = Some(exit_code);
                session.wire.input_closed = true;
            }
            NativeTerminalOutputEvent::End => {
                session.wire.state = ProfileSettingsState::Closed;
                session.control = None;
            }
            NativeTerminalOutputEvent::Output { .. } => (),
        }
        Ok(())
    }

    pub(crate) fn mark_unknown(&self, id: &str) {
        if let Some(session) = self
            .sessions
            .lock()
            .iter_mut()
            .find(|session| session.wire.session_id == id)
            && session.wire.state != ProfileSettingsState::Closed
        {
            session.wire.state = ProfileSettingsState::Unknown;
        }
    }

    pub(crate) fn start_outcome_guard(self: &Arc<Self>, id: &str) -> ProfileSettingsStartGuard {
        ProfileSettingsStartGuard {
            manager: self.clone(),
            id: id.into(),
            accepted: false,
            dispatched: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub fn session(
        &self,
        owner: &str,
        id: &str,
    ) -> Result<ProfileSettingsSession, ProfileSettingsError> {
        let sessions = self.sessions.lock();
        Ok(owned(&sessions, owner, id)?.wire.clone())
    }

    pub fn list(&self, owner: &str) -> Vec<ProfileSettingsSession> {
        self.sessions
            .lock()
            .iter()
            .filter(|session| session.owner == owner)
            .map(|session| session.wire.clone())
            .collect()
    }

    pub fn unresolved_profile_ids(&self) -> HashSet<String> {
        self.sessions
            .lock()
            .iter()
            .filter(|session| session.wire.state != ProfileSettingsState::Closed)
            .map(|session| session.wire.profile_id.clone())
            .collect()
    }

    pub fn output(
        &self,
        owner: &str,
        id: &str,
        after: u64,
    ) -> Result<NativeTerminalOutputPageWire, ProfileSettingsError> {
        let sessions = self.sessions.lock();
        owned(&sessions, owner, id)?;
        let records = self
            .output
            .page_after(id, after, 16)
            .map_err(|_| ProfileSettingsError::Unavailable)?;
        Ok(NativeTerminalOutputPageWire {
            next_sequence: records
                .last()
                .map_or(after, |record| record.sequence)
                .to_string(),
            events: records
                .iter()
                .map(|record| NativeTerminalOutputRecordWire::from_record(id, record))
                .collect::<Result<_, _>>()
                .map_err(|_| ProfileSettingsError::Unavailable)?,
        })
    }

    fn input_control(
        &self,
        owner: &str,
        id: &str,
    ) -> Result<Arc<dyn ProfileSettingsControl>, ProfileSettingsError> {
        let sessions = self.sessions.lock();
        let session = owned(&sessions, owner, id)?;
        if session.wire.state != ProfileSettingsState::Running || session.wire.input_closed {
            return Err(ProfileSettingsError::Conflict);
        }
        session
            .control
            .clone()
            .ok_or(ProfileSettingsError::Unavailable)
    }

    pub async fn send_input(
        &self,
        owner: &str,
        id: &str,
        bytes: Vec<u8>,
    ) -> Result<(), ProfileSettingsError> {
        if bytes.is_empty() || bytes.len() > 64 * 1024 {
            return Err(ProfileSettingsError::Invalid);
        }
        let control = self.input_control(owner, id)?;
        let mut outcome = OperationGuard {
            manager: self,
            id,
            confirmed: false,
        };
        let result = tokio::time::timeout(Duration::from_secs(10), control.send_input(bytes))
            .await
            .unwrap_or(Err(ProfileSettingsError::Unavailable));
        if result.is_err() {
            self.mark_unknown(id);
        }
        outcome.confirmed = true;
        result
    }

    pub async fn finish_input(&self, owner: &str, id: &str) -> Result<(), ProfileSettingsError> {
        let control = self.input_control(owner, id)?;
        let mut outcome = OperationGuard {
            manager: self,
            id,
            confirmed: false,
        };
        {
            let mut sessions = self.sessions.lock();
            sessions
                .iter_mut()
                .find(|session| session.wire.session_id == id)
                .ok_or(ProfileSettingsError::NotFound)?
                .wire
                .input_closed = true;
        }
        let result = tokio::time::timeout(Duration::from_secs(10), control.finish_input())
            .await
            .unwrap_or(Err(ProfileSettingsError::Unavailable));
        if result.is_err() {
            self.mark_unknown(id);
        }
        outcome.confirmed = true;
        result
    }

    pub async fn cancel(&self, owner: &str, id: &str) -> Result<(), ProfileSettingsError> {
        let control = {
            let mut sessions = self.sessions.lock();
            owned(&sessions, owner, id)?;
            let session = sessions
                .iter_mut()
                .find(|session| session.wire.session_id == id)
                .ok_or(ProfileSettingsError::NotFound)?;
            if session.wire.state == ProfileSettingsState::Closed || session.cancel_requested {
                return Ok(());
            }
            let control = session
                .control
                .clone()
                .ok_or(ProfileSettingsError::Unavailable)?;
            session.cancel_requested = true;
            session.wire.state = ProfileSettingsState::Cancelling;
            control
        };
        let mut outcome = OperationGuard {
            manager: self,
            id,
            confirmed: false,
        };
        let result = tokio::time::timeout(Duration::from_secs(10), control.cancel())
            .await
            .unwrap_or(Err(ProfileSettingsError::Unavailable));
        if result.is_err() {
            self.mark_unknown(id);
        }
        outcome.confirmed = true;
        result
    }

    pub async fn shutdown(&self, owner: &str, timeout: Duration) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        for session in self
            .list(owner)
            .into_iter()
            .filter(|session| session.state != ProfileSettingsState::Closed)
        {
            let _ =
                tokio::time::timeout_at(deadline, self.cancel(owner, &session.session_id)).await;
        }
        loop {
            let active = self
                .list(owner)
                .into_iter()
                .filter(|session| session.state != ProfileSettingsState::Closed)
                .map(|session| session.session_id)
                .collect::<Vec<_>>();
            if active.is_empty() {
                return active;
            }
            if tokio::time::Instant::now() >= deadline {
                for id in &active {
                    self.mark_unknown(id);
                }
                return active;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn owned<'a>(
    sessions: &'a [Session],
    owner: &str,
    id: &str,
) -> Result<&'a Session, ProfileSettingsError> {
    let session = sessions
        .iter()
        .find(|session| session.wire.session_id == id)
        .ok_or(ProfileSettingsError::NotFound)?;
    if session.owner != owner {
        return Err(ProfileSettingsError::OwnerMismatch);
    }
    Ok(session)
}
struct StreamGuard {
    manager: Arc<ProfileSettingsTerminals>,
    id: String,
}
impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.manager.mark_unknown(&self.id);
    }
}

struct OperationGuard<'a> {
    manager: &'a ProfileSettingsTerminals,
    id: &'a str,
    confirmed: bool,
}
impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        if !self.confirmed {
            self.manager.mark_unknown(self.id);
        }
    }
}

pub(crate) struct ProfileSettingsStartGuard {
    manager: Arc<ProfileSettingsTerminals>,
    id: String,
    accepted: bool,
    dispatched: Arc<std::sync::atomic::AtomicBool>,
}
impl ProfileSettingsStartGuard {
    pub(crate) fn accepted(mut self) {
        self.accepted = true;
    }
    pub(crate) fn dispatch_marker(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.dispatched.clone()
    }
}
impl Drop for ProfileSettingsStartGuard {
    fn drop(&mut self) {
        if self.accepted {
            return;
        }
        if self.dispatched.load(std::sync::atomic::Ordering::Acquire) {
            self.manager.mark_unknown(&self.id);
        } else {
            self.manager
                .sessions
                .lock()
                .retain(|session| session.wire.session_id != self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Control {
        writes: AtomicUsize,
        cancels: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ProfileSettingsControl for Control {
        async fn send_input(&self, _: Vec<u8>) -> Result<(), ProfileSettingsError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn finish_input(&self) -> Result<(), ProfileSettingsError> {
            Ok(())
        }
        async fn cancel(&self) -> Result<(), ProfileSettingsError> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn reserve(manager: &ProfileSettingsTerminals, owner: &str) -> ProfileSettingsSession {
        manager
            .reserve(owner, "profile", 3, 4, "registry.example/coder@sha256:aaa")
            .unwrap()
    }

    #[tokio::test]
    async fn shell_operations_are_owner_bound_and_end_requires_real_stream_completion() {
        let manager = ProfileSettingsTerminals::new();
        let session = reserve(&manager, "main");
        let control = std::sync::Arc::new(Control::default());
        manager
            .accept(&session.session_id, 42, control.clone())
            .unwrap();
        assert!(
            manager
                .send_input("other", &session.session_id, vec![1])
                .await
                .is_err()
        );
        assert!(manager.cancel("other", &session.session_id).await.is_err());
        assert!(manager.output("other", &session.session_id, 0).is_err());
        assert!(
            manager
                .send_input("main", &session.session_id, vec![0; 65537])
                .await
                .is_err()
        );
        manager
            .send_input("main", &session.session_id, vec![0; 65536])
            .await
            .unwrap();
        assert_eq!(control.writes.load(Ordering::SeqCst), 1);
        assert!(
            manager
                .record(&session.session_id, NativeTerminalOutputEvent::End)
                .is_err()
        );
        manager
            .record(
                &session.session_id,
                NativeTerminalOutputEvent::Exit {
                    exit_code: 0,
                    execution_time_ms: 8,
                    sandbox_id: "vm".into(),
                },
            )
            .unwrap();
        assert_eq!(
            manager.session("main", &session.session_id).unwrap().state,
            ProfileSettingsState::Running
        );
        manager
            .record(&session.session_id, NativeTerminalOutputEvent::End)
            .unwrap();
        assert_eq!(
            manager.session("main", &session.session_id).unwrap().state,
            ProfileSettingsState::Closed
        );
        assert!(manager.unresolved_profile_ids().is_empty());
        assert!(
            manager
                .send_input("main", &session.session_id, vec![1])
                .await
                .is_err()
        );
        assert!(
            manager
                .output("main", &session.session_id, 0)
                .unwrap()
                .events
                .len()
                == 2
        );
    }

    #[tokio::test]
    async fn cancellation_is_a_request_and_unknown_acceptance_keeps_the_profile_busy() {
        let manager = ProfileSettingsTerminals::new();
        let session = reserve(&manager, "main");
        assert!(manager.reserve("main", "profile", 3, 4, "image").is_err());
        manager.mark_unknown(&session.session_id);
        assert_eq!(
            manager.unresolved_profile_ids(),
            std::collections::HashSet::from(["profile".into()])
        );
        assert!(manager.cancel("main", &session.session_id).await.is_err());
        let control = std::sync::Arc::new(Control::default());
        manager
            .accept(&session.session_id, 42, control.clone())
            .unwrap();
        manager.cancel("main", &session.session_id).await.unwrap();
        manager.cancel("main", &session.session_id).await.unwrap();
        assert_eq!(control.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(
            manager.session("main", &session.session_id).unwrap().state,
            ProfileSettingsState::Cancelling
        );
        assert!(
            manager
                .send_input("main", &session.session_id, vec![1])
                .await
                .is_err()
        );
        manager.mark_unknown(&session.session_id);
        assert_eq!(
            manager.session("main", &session.session_id).unwrap().state,
            ProfileSettingsState::Unknown
        );
        assert!(!manager.unresolved_profile_ids().is_empty());
    }

    #[test]
    fn abandoned_start_and_capacity_preserve_existing_active_profiles() {
        let manager = Arc::new(ProfileSettingsTerminals::new());
        let first = reserve(&manager, "main");
        let start = manager.start_outcome_guard(&first.session_id);
        start.dispatch_marker().store(true, Ordering::Release);
        drop(start);
        assert_eq!(
            manager.session("main", &first.session_id).unwrap().state,
            ProfileSettingsState::Unknown
        );
        for index in 1..32 {
            manager
                .reserve("main", &format!("profile-{index}"), 1, 1, "image")
                .unwrap();
        }
        assert!(manager.reserve("other", "extra", 1, 1, "image").is_err());
        assert_eq!(manager.list("main").len(), 32);
        assert_eq!(manager.unresolved_profile_ids().len(), 32);
    }

    #[tokio::test]
    async fn shutdown_deadline_does_not_close_unknown_sessions_or_other_owners() {
        let manager = ProfileSettingsTerminals::new();
        let first = reserve(&manager, "main");
        let other = manager
            .reserve("other", "another-profile", 1, 1, "image")
            .unwrap();
        assert_eq!(
            manager.shutdown("main", Duration::ZERO).await,
            vec![first.session_id.clone()]
        );
        assert_eq!(
            manager.session("main", &first.session_id).unwrap().state,
            ProfileSettingsState::Unknown
        );
        assert_eq!(
            manager.session("other", &other.session_id).unwrap().state,
            ProfileSettingsState::Starting
        );
        assert_eq!(manager.unresolved_profile_ids().len(), 2);
    }

    struct PendingControl {
        entered: tokio::sync::Notify,
    }
    #[async_trait::async_trait]
    impl ProfileSettingsControl for PendingControl {
        async fn send_input(&self, _: Vec<u8>) -> Result<(), ProfileSettingsError> {
            self.entered.notify_one();
            std::future::pending().await
        }
        async fn finish_input(&self) -> Result<(), ProfileSettingsError> {
            std::future::pending().await
        }
        async fn cancel(&self) -> Result<(), ProfileSettingsError> {
            std::future::pending().await
        }
    }
    #[tokio::test]
    async fn dropped_input_request_records_unknown_delivery_and_disables_further_input() {
        let manager = Arc::new(ProfileSettingsTerminals::new());
        let session = reserve(&manager, "main");
        let control = Arc::new(PendingControl {
            entered: tokio::sync::Notify::new(),
        });
        manager
            .accept(&session.session_id, 42, control.clone())
            .unwrap();
        let writer = manager.clone();
        let id = session.session_id.clone();
        let task = tokio::spawn(async move { writer.send_input("main", &id, vec![1]).await });
        control.entered.notified().await;
        task.abort();
        let _ = task.await;
        assert_eq!(
            manager.session("main", &session.session_id).unwrap().state,
            ProfileSettingsState::Unknown
        );
        assert!(
            manager
                .send_input("main", &session.session_id, vec![1])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn observed_shell_requires_exit_end_and_clean_eof_and_retains_actual_job_id() {
        use crate::grpc::proto::jobworkerp::runner::sandbox::{
            SandboxExecExit, SandboxExecResult, sandbox_exec_result::Result as SandboxResult,
        };
        use crate::jobworkerp::coding_agent::{
            HostWorkerContext, NativeProfileSettingsWorkerSpec, NativeWorkspaceBinding,
        };
        use jobworkerp_client::jobworkerp::data::{ResultOutputItem, result_output_item::Item};
        use prost::Message;
        let exit = ResultOutputItem {
            item: Some(Item::Data(
                SandboxExecResult {
                    result: Some(SandboxResult::Exit(SandboxExecExit {
                        exit_code: 0,
                        execution_time_ms: 1,
                        sandbox_id: "vm".into(),
                    })),
                }
                .encode_to_vec(),
            )),
        };
        let end = ResultOutputItem {
            item: Some(Item::End(Default::default())),
        };
        for (items, trailing_error, expected, drift, omit_id) in [
            (
                vec![exit.clone(), end.clone()],
                false,
                ProfileSettingsState::Closed,
                false,
                false,
            ),
            (vec![], false, ProfileSettingsState::Unknown, false, false),
            (
                vec![exit.clone()],
                false,
                ProfileSettingsState::Unknown,
                false,
                false,
            ),
            (
                vec![exit.clone(), end.clone()],
                true,
                ProfileSettingsState::Unknown,
                false,
                false,
            ),
            (vec![], false, ProfileSettingsState::Unknown, true, false),
            (vec![], false, ProfileSettingsState::Unknown, false, true),
        ] {
            let fixture = rpc_fixture::RpcFixture::default();
            fixture.set_job_result_items(items);
            if trailing_error {
                fixture.fail_stream_after_output();
            }
            let server = rpc_fixture::start_server(fixture.clone()).await;
            let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
            let matched = handle.ensure_native_profile_settings_worker(NativeProfileSettingsWorkerSpec {
                native_profile_id: "profile".into(), use_static: false,
                workspace_binding: NativeWorkspaceBinding { canonical_host_root: "/tmp/profile-settings-workspace".into(), guest_root: "/workspace".into() },
                runner_settings_json: serde_json::json!({
                    "vm": { "image": "registry.example/coder@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "cpus": 1, "memory_mib": 2048, "root_disk_mib": 8192,
                        "mounts": [{"host_path":"/tmp/profile-settings-workspace", "guest_path":"/workspace", "writable":true, "executable":true}], "env":{"HOME":"/home/agent"} },
                    "allowed_images":["registry.example/coder@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
                    "allowed_host_mounts":[{"host_root":"/tmp/profile-settings-workspace", "allow_write":true, "allow_exec":true}], "network":{"enabled":false}
                }),
                host_context: HostWorkerContext { microsandbox_sdk_home: "/tmp/test-sdk".into(), backend_config_path: "/tmp/test-sdk/config.json".into() },
            }).await.unwrap();
            let manager = Arc::new(ProfileSettingsTerminals::new());
            let session = reserve(&manager, "main");
            let start = manager.start_outcome_guard(&session.session_id);
            let marker = start.dispatch_marker();
            if drift {
                fixture.mutate_first_worker(|worker| worker.use_static = !worker.use_static);
            }
            if omit_id {
                fixture.omit_next_job_id();
            }
            let result = handle
                .start_native_profile_settings_terminal_with_dispatch(&matched, || {
                    marker.store(true, Ordering::Release)
                })
                .await;
            if drift || omit_id {
                assert!(result.is_err());
                drop(start);
                assert_eq!(manager.unresolved_profile_ids().is_empty(), drift);
                assert_eq!(manager.list("main").is_empty(), drift);
                assert_eq!(
                    fixture.counts().enqueue_with_client_stream_calls,
                    usize::from(omit_id)
                );
                continue;
            }
            let stream = result.unwrap();
            let actual_job = stream.job_id().value.to_string();
            manager
                .observe(&session.session_id, handle, stream)
                .unwrap();
            start.accepted();
            tokio::time::timeout(Duration::from_secs(2), async {
                while manager.session("main", &session.session_id).unwrap().state != expected {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                manager.session("main", &session.session_id).unwrap().job_id,
                Some(actual_job)
            );
            assert_eq!(
                manager.unresolved_profile_ids().is_empty(),
                expected == ProfileSettingsState::Closed
            );
            assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 1);
        }
    }
}
