use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use jobworkerp_client::jobworkerp::data::JobId;
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, broadcast};

use crate::{
    coding_agent::{AgentProfileSnapshot, ResolvedRepository},
    jobworkerp::{
        JobworkerpHandle,
        coding_agent::{CodingAgentEvent, CodingAgentInput, CodingAgentStream, NativeWorkerMatch},
    },
};

use super::{
    JobAcceptance, NativeCancelClaim, NativeSessionContext, NativeSessionRecord,
    NativeTerminalObservation, NativeTerminalOutputError, NativeTerminalOutputEvent,
    NativeTerminalOutputRecord, NativeTerminalOutputStore, NativeTerminalStore,
    NativeTerminalStoreError, SessionState, StartClaimOutcome, is_canonical_uuid,
    output::MAX_OUTPUT_FRAME_BYTES,
};

const NATIVE_OUTPUT_SUBSCRIBER_CAPACITY: usize = 64;
const WORKER_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
const AUTHORITY_READ_TIMEOUT: Duration = Duration::from_secs(10);
const NATIVE_CANCEL_RPC_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct NativeAuthoritySnapshot {
    pub canonical_issue: super::CanonicalIssueSnapshot,
    pub repository: ResolvedRepository,
    pub profile: AgentProfileSnapshot,
    pub settings_revision: u64,
}

impl NativeAuthoritySnapshot {
    /// Construct from host-fetched canonical Issue, repository, and settings data.
    pub fn from_host(
        canonical_issue: super::CanonicalIssueSnapshot,
        repository: ResolvedRepository,
        profile: AgentProfileSnapshot,
        settings_revision: u64,
    ) -> Self {
        Self {
            canonical_issue,
            repository,
            profile,
            settings_revision,
        }
    }
}

#[derive(Clone, Copy, Debug, Error)]
pub enum NativeAuthorityError {
    #[error("canonical native session authority is unavailable")]
    Unavailable,
}

/// Implemented only by the host adapter that rereads current canonical Issue and profile data.
#[async_trait]
#[allow(clippy::double_must_use)]
pub trait NativeSessionAuthority: Send + Sync {
    async fn read_current(
        &self,
        expected: &NativeSessionContext,
    ) -> Result<NativeAuthoritySnapshot, NativeAuthorityError>;
}

#[derive(Debug, Error)]
pub enum NativeTerminalManagerError {
    #[error("native terminal request is invalid")]
    InvalidRequest,
    #[error("native terminal preview or session was not found")]
    NotFound,
    #[error("native terminal resource belongs to another window")]
    OwnerMismatch,
    #[error("native terminal preview or session conflicts with its immutable record")]
    Conflict,
    #[error("native terminal admission is closed")]
    AdmissionClosed,
    #[error("native terminal worker identity does not match the prepared context")]
    WorkerMismatch,
    #[error("canonical native session authority could not be fetched")]
    AuthorityUnavailable,
    #[error("canonical Issue, repository, or profile changed after preparation")]
    AuthorityChanged,
    #[error("native terminal session is not accepting input")]
    SessionNotRunning,
    #[error("native terminal output subscriber must resynchronize from the live display buffer")]
    OutputGap,
    #[error("native terminal job transport failed; acceptance or outcome may be unknown")]
    Transport,
    #[error("native terminal store operation failed")]
    Store(NativeTerminalStoreError),
    #[error("native terminal output operation failed")]
    Output(NativeTerminalOutputError),
}

#[derive(Clone)]
struct StagedSession {
    context: NativeSessionContext,
    matched: Option<NativeWorkerMatch>,
}

struct ManagedSession {
    native_job_id: Option<i64>,
    input: Option<CodingAgentInput>,
    output: broadcast::Sender<NativeTerminalOutputRecord>,
}

struct ManagerInner {
    store: Arc<NativeTerminalStore>,
    output: Arc<NativeTerminalOutputStore>,
    jobworkerp: JobworkerpHandle,
    authority: Arc<dyn NativeSessionAuthority>,
    admission: Arc<AsyncMutex<()>>,
    staged: Mutex<HashMap<String, StagedSession>>,
    sessions: Arc<Mutex<HashMap<String, ManagedSession>>>,
    starting: Arc<Mutex<HashSet<String>>>,
    ownership_transition: Mutex<()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeSessionManagementOwnership {
    None,
    Starting,
    Managed,
}

struct StartingSessionReservation {
    starting: Arc<Mutex<HashSet<String>>>,
    session_id: String,
}

impl StartingSessionReservation {
    fn acquire(
        starting: Arc<Mutex<HashSet<String>>>,
        session_id: &str,
    ) -> Result<Self, NativeTerminalManagerError> {
        let inserted = starting
            .lock()
            .map_err(|_| NativeTerminalManagerError::Conflict)?
            .insert(session_id.to_owned());
        if !inserted {
            return Err(NativeTerminalManagerError::Conflict);
        }
        Ok(Self {
            starting,
            session_id: session_id.to_owned(),
        })
    }
}

impl Drop for StartingSessionReservation {
    fn drop(&mut self) {
        self.starting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.session_id);
    }
}

#[derive(Clone)]
pub struct NativeTerminalManager {
    inner: Arc<ManagerInner>,
}

pub(crate) struct NativeManagerAdmissionGuard {
    _guard: OwnedMutexGuard<()>,
    manager_identity: usize,
}

impl std::fmt::Debug for NativeTerminalManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeTerminalManager")
            .finish_non_exhaustive()
    }
}

pub struct NativeTerminalAttachment {
    pub record: NativeSessionRecord,
    pub events: Vec<NativeTerminalOutputRecord>,
    pub next_sequence: u64,
    pub subscription: NativeTerminalSubscription,
}

pub struct NativeTerminalSubscription {
    receiver: broadcast::Receiver<NativeTerminalOutputRecord>,
    last_sequence: u64,
}

impl NativeTerminalSubscription {
    /// Receive a durable event in sequence; callers recover lag from `output_page`.
    pub async fn recv(&mut self) -> Result<NativeTerminalOutputRecord, NativeTerminalManagerError> {
        loop {
            let record = match self.receiver.recv().await {
                Ok(record) => record,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    return Err(NativeTerminalManagerError::OutputGap);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(NativeTerminalManagerError::OutputGap);
                }
            };
            if record.sequence <= self.last_sequence {
                continue;
            }
            if record.sequence != self.last_sequence.saturating_add(1) {
                return Err(NativeTerminalManagerError::OutputGap);
            }
            self.last_sequence = record.sequence;
            return Ok(record);
        }
    }
}

impl NativeTerminalManager {
    /// Dependencies are explicit: no implicit authority source or renderer-provided worker identity.
    pub fn new(
        store: Arc<NativeTerminalStore>,
        output: Arc<NativeTerminalOutputStore>,
        jobworkerp: JobworkerpHandle,
        authority: Arc<dyn NativeSessionAuthority>,
        shared_admission: Arc<AsyncMutex<()>>,
    ) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                store,
                output,
                jobworkerp,
                authority,
                admission: shared_admission,
                staged: Mutex::new(HashMap::new()),
                sessions: Arc::new(Mutex::new(HashMap::new())),
                starting: Arc::new(Mutex::new(HashSet::new())),
                ownership_transition: Mutex::new(()),
            }),
        }
    }

    /// The Settings compare-and-swap path must use this same admission lock.
    pub fn admission_lock(&self) -> Arc<AsyncMutex<()>> {
        Arc::clone(&self.inner.admission)
    }

    pub(crate) async fn lock_admission(&self) -> NativeManagerAdmissionGuard {
        NativeManagerAdmissionGuard {
            _guard: Arc::clone(&self.inner.admission).lock_owned().await,
            manager_identity: Arc::as_ptr(&self.inner) as usize,
        }
    }

    /// Acquire the same lock used by Settings admission, then recheck the
    /// shared quit flag. A caller queued behind shutdown must not proceed to a
    /// durable claim or enqueue after the barrier releases this mutex.
    pub(crate) async fn lock_admission_if_accepting(
        &self,
        accepting: &AtomicBool,
    ) -> Result<NativeManagerAdmissionGuard, NativeTerminalManagerError> {
        let guard = self.lock_admission().await;
        if !accepting.load(Ordering::Acquire) {
            return Err(NativeTerminalManagerError::AdmissionClosed);
        }
        Ok(guard)
    }

    /// Stage only a host-prepared immutable context and a typed, verified worker match.
    pub fn register_preview(
        &self,
        context: NativeSessionContext,
        matched: &NativeWorkerMatch,
    ) -> Result<(), NativeTerminalManagerError> {
        context
            .validate()
            .map_err(|_| NativeTerminalManagerError::InvalidRequest)?;
        validate_worker_binding(&context, matched)?;
        self.inner.output.ensure_disjoint_from_workspace(
            &context.workspace_mount_binding().canonical_host_root,
        )?;
        for mount in &context.profile().persistent_mounts {
            self.inner
                .output
                .ensure_disjoint_from_workspace(&mount.host_path)?;
        }
        if let Some(overlay) = &context.profile().managed_home_overlay {
            self.inner
                .output
                .ensure_disjoint_from_workspace(&overlay.parent.host_path)?;
            for child in &overlay.children {
                self.inner
                    .output
                    .ensure_disjoint_from_workspace(child.host_path())?;
            }
        }
        self.inner
            .store
            .persist_preview(context.preview(), context.owner_window_label())?;

        let preview_id = context.preview().preview_id().to_owned();
        let mut staged = self
            .inner
            .staged
            .lock()
            .map_err(|_| NativeTerminalManagerError::Conflict)?;
        match staged.get(&preview_id) {
            Some(existing)
                if existing.context == context
                    && existing.matched.as_ref().is_some_and(|old| {
                        old.worker_id() == matched.worker_id()
                            && old.runner_id() == matched.runner_id()
                            && old.settings_digest() == matched.settings_digest()
                    }) =>
            {
                Ok(())
            }
            Some(_) => Err(NativeTerminalManagerError::Conflict),
            None => {
                staged.insert(
                    preview_id,
                    StagedSession {
                        context,
                        matched: Some(matched.clone()),
                    },
                );
                Ok(())
            }
        }
    }

    /// Revalidate canonical host data, durably claim, and enqueue exactly once.
    pub async fn start(
        &self,
        owner_window_label: &str,
        preview_id: &str,
        preview_digest: &str,
    ) -> Result<NativeSessionRecord, NativeTerminalManagerError> {
        let admission = self.lock_admission().await;
        self.start_admitted(&admission, owner_window_label, preview_id, preview_digest)
            .await
    }

    /// Start while the caller retains this exact manager's shared Settings
    /// admission guard. The Doctor uses this seam to keep fresh lease checks,
    /// the durable claim, and the stream-open RPC in one admission section.
    pub(crate) async fn start_admitted(
        &self,
        admission: &NativeManagerAdmissionGuard,
        owner_window_label: &str,
        preview_id: &str,
        preview_digest: &str,
    ) -> Result<NativeSessionRecord, NativeTerminalManagerError> {
        if !is_canonical_uuid(preview_id) {
            return Err(NativeTerminalManagerError::InvalidRequest);
        }
        if admission.manager_identity != Arc::as_ptr(&self.inner) as usize {
            return Err(NativeTerminalManagerError::Conflict);
        }

        // A previous manager may have lost its RAM staging map after claiming the durable session.
        if let Some(record) = self
            .inner
            .store
            .list_sessions_for_window(owner_window_label)?
            .into_iter()
            .find(|record| record.preview_id == preview_id)
        {
            if record.preview_digest != preview_digest {
                return Err(NativeTerminalManagerError::Conflict);
            }
            return Ok(record);
        }

        let staged = {
            let staged = self
                .inner
                .staged
                .lock()
                .map_err(|_| NativeTerminalManagerError::Conflict)?;
            staged
                .get(preview_id)
                .cloned()
                .ok_or(NativeTerminalManagerError::NotFound)?
        };
        if staged.context.owner_window_label() != owner_window_label {
            return Err(NativeTerminalManagerError::OwnerMismatch);
        }
        if staged.context.preview().preview_digest() != preview_digest {
            return Err(NativeTerminalManagerError::Conflict);
        }
        let matched = staged
            .matched
            .as_ref()
            .ok_or(NativeTerminalManagerError::Conflict)?;
        validate_worker_binding(&staged.context, matched)?;

        let current = tokio::time::timeout(
            AUTHORITY_READ_TIMEOUT,
            self.inner.authority.read_current(&staged.context),
        )
        .await
        .map_err(|_| NativeTerminalManagerError::AuthorityUnavailable)?
        .map_err(|_| NativeTerminalManagerError::AuthorityUnavailable)?;
        if &current.canonical_issue != staged.context.canonical_issue()
            || current.repository != *staged.context.repository()
            || current.profile != *staged.context.profile()
            || current.settings_revision != staged.context.preview().expected_settings_revision()
        {
            return Err(NativeTerminalManagerError::AuthorityChanged);
        }

        let binding = staged
            .context
            .preview()
            .worker_binding()
            .ok_or(NativeTerminalManagerError::WorkerMismatch)?;
        let worker_id = tokio::time::timeout(
            WORKER_LOOKUP_TIMEOUT,
            self.inner
                .jobworkerp
                .worker_id_by_name(&binding.worker_name),
        )
        .await
        .map_err(|_| NativeTerminalManagerError::Transport)?
        .map_err(|_| NativeTerminalManagerError::Transport)?;
        if worker_id != Some(binding.worker_id) {
            return Err(NativeTerminalManagerError::WorkerMismatch);
        }

        let (claim, mut starting_owner) = {
            let _ownership_transition = self
                .inner
                .ownership_transition
                .lock()
                .map_err(|_| NativeTerminalManagerError::Conflict)?;
            let claim = self
                .inner
                .store
                .claim_start(staged.context.clone(), owner_window_label)?;
            let starting_owner = match &claim {
                StartClaimOutcome::Claimed(record) => Some(StartingSessionReservation::acquire(
                    Arc::clone(&self.inner.starting),
                    &record.session_id,
                )?),
                StartClaimOutcome::Existing(_) => None,
            };
            (claim, starting_owner)
        };
        match claim {
            StartClaimOutcome::Existing(record) => Ok(record),
            StartClaimOutcome::Claimed(claimed_record) => {
                let _starting_owner = starting_owner
                    .take()
                    .ok_or(NativeTerminalManagerError::Conflict)?;
                let stream = self
                    .inner
                    .jobworkerp
                    .start_native_interactive_session(
                        matched,
                        staged.context.launch_plan().command(),
                        staged.context.launch_plan().args(),
                        staged.context.launch_plan().working_directory(),
                    )
                    .await;
                let stream = match stream {
                    Ok(stream) => stream,
                    Err(_) => {
                        self.inner
                            .store
                            .mark_start_unknown(staged.context.session_id())?;
                        self.drop_staged_match(preview_id);
                        return Err(NativeTerminalManagerError::Transport);
                    }
                };

                let native_job_id = stream.job_id().value;
                if native_job_id <= 0 {
                    let _ = self
                        .inner
                        .store
                        .mark_start_unknown(staged.context.session_id());
                    let (sender, _) = broadcast::channel(NATIVE_OUTPUT_SUBSCRIBER_CAPACITY);
                    if let Ok(mut sessions) = self.inner.sessions.lock() {
                        sessions.insert(
                            claimed_record.session_id.clone(),
                            ManagedSession {
                                native_job_id: None,
                                input: None,
                                output: sender.clone(),
                            },
                        );
                    }
                    spawn_output_pump(
                        Arc::clone(&self.inner.store),
                        Arc::clone(&self.inner.output),
                        Arc::clone(&self.inner.sessions),
                        claimed_record.session_id,
                        stream,
                        sender,
                    );
                    self.drop_staged_match(preview_id);
                    return Err(NativeTerminalManagerError::Transport);
                }
                let acceptance_result = self
                    .inner
                    .store
                    .record_job_accepted(staged.context.session_id(), native_job_id);
                let (record, acceptance, acceptance_error) = match acceptance_result {
                    Ok(acceptance) => match self
                        .inner
                        .store
                        .get_session_for_window(staged.context.session_id(), owner_window_label)
                    {
                        Ok(Some(record)) => (record, Some(acceptance), None),
                        Ok(None) => {
                            let _ = self
                                .inner
                                .store
                                .mark_start_unknown(staged.context.session_id());
                            (
                                uncertain_record(claimed_record, native_job_id),
                                Some(acceptance),
                                Some(NativeTerminalStoreError::NotFound),
                            )
                        }
                        Err(error) => {
                            let _ = self
                                .inner
                                .store
                                .mark_start_unknown(staged.context.session_id());
                            (
                                uncertain_record(claimed_record, native_job_id),
                                Some(acceptance),
                                Some(error),
                            )
                        }
                    },
                    Err(error) => {
                        let _ = self
                            .inner
                            .store
                            .mark_start_unknown(staged.context.session_id());
                        (
                            uncertain_record(claimed_record, native_job_id),
                            None,
                            Some(error),
                        )
                    }
                };
                let (sender, _) = broadcast::channel(NATIVE_OUTPUT_SUBSCRIBER_CAPACITY);
                let input = (acceptance.is_some()
                    && record.state == SessionState::Running
                    && !record.cancel_requested)
                    .then(|| stream.input_handle())
                    .flatten();
                self.inner
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        record.session_id.clone(),
                        ManagedSession {
                            native_job_id: Some(native_job_id),
                            input,
                            output: sender.clone(),
                        },
                    );
                spawn_output_pump(
                    Arc::clone(&self.inner.store),
                    Arc::clone(&self.inner.output),
                    Arc::clone(&self.inner.sessions),
                    record.session_id.clone(),
                    stream,
                    sender,
                );
                self.drop_staged_match(preview_id);

                if let Some(JobAcceptance {
                    cancel_dispatch_job_id: Some(job_id),
                }) = acceptance
                {
                    if self.dispatch_cancel(job_id).await.is_err() {
                        let _ = self
                            .inner
                            .store
                            .mark_start_unknown(staged.context.session_id());
                        return Err(NativeTerminalManagerError::Transport);
                    }
                } else if let Some(error) = acceptance_error {
                    return Err(NativeTerminalManagerError::Store(error));
                }
                self.inner
                    .store
                    .get_session_for_window(staged.context.session_id(), owner_window_label)?
                    .ok_or(NativeTerminalManagerError::NotFound)
            }
        }
    }

    /// Return only the in-memory owner-bound preview candidate that was already
    /// prepared and matched by the native host. This is not a renderer input.
    pub(crate) fn staged_native_candidate(
        &self,
        owner_window_label: &str,
        preview_id: &str,
        preview_digest: &str,
    ) -> Result<(NativeSessionContext, NativeWorkerMatch), NativeTerminalManagerError> {
        if !is_canonical_uuid(preview_id) {
            return Err(NativeTerminalManagerError::InvalidRequest);
        }
        let staged = self
            .inner
            .staged
            .lock()
            .map_err(|_| NativeTerminalManagerError::Conflict)?
            .get(preview_id)
            .cloned()
            .ok_or(NativeTerminalManagerError::NotFound)?;
        if staged.context.owner_window_label() != owner_window_label {
            return Err(NativeTerminalManagerError::OwnerMismatch);
        }
        if staged.context.preview().preview_digest() != preview_digest {
            return Err(NativeTerminalManagerError::Conflict);
        }
        staged
            .context
            .validate()
            .map_err(|_| NativeTerminalManagerError::InvalidRequest)?;
        let matched = staged
            .matched
            .as_ref()
            .ok_or(NativeTerminalManagerError::WorkerMismatch)?;
        validate_worker_binding(&staged.context, matched)?;
        Ok((staged.context, matched.clone()))
    }

    /// Reread canonical Issue, repository, and profile authority while the
    /// caller holds this manager's admission guard. The staged clone alone is
    /// not fresh authority and must never be used to validate a Doctor lease.
    pub(crate) async fn staged_native_candidate_with_current_authority(
        &self,
        admission: &NativeManagerAdmissionGuard,
        owner_window_label: &str,
        preview_id: &str,
        preview_digest: &str,
    ) -> Result<(NativeSessionContext, NativeWorkerMatch), NativeTerminalManagerError> {
        if admission.manager_identity != Arc::as_ptr(&self.inner) as usize {
            return Err(NativeTerminalManagerError::Conflict);
        }
        let (context, matched) =
            self.staged_native_candidate(owner_window_label, preview_id, preview_digest)?;
        let current = tokio::time::timeout(
            AUTHORITY_READ_TIMEOUT,
            self.inner.authority.read_current(&context),
        )
        .await
        .map_err(|_| NativeTerminalManagerError::AuthorityUnavailable)?
        .map_err(|_| NativeTerminalManagerError::AuthorityUnavailable)?;
        if current.canonical_issue != *context.canonical_issue()
            || current.repository != *context.repository()
            || current.profile != *context.profile()
            || current.settings_revision != context.preview().expected_settings_revision()
        {
            return Err(NativeTerminalManagerError::AuthorityChanged);
        }

        // The synchronous staging API can be called independently of this
        // admission lock. Ensure it did not replace or remove the candidate
        // while the asynchronous authority read was in flight.
        let (current_context, current_match) =
            self.staged_native_candidate(owner_window_label, preview_id, preview_digest)?;
        if current_context != context
            || current_match.worker_id() != matched.worker_id()
            || current_match.runner_id() != matched.runner_id()
            || current_match.settings_digest() != matched.settings_digest()
            || current_match.method_schema_digest() != matched.method_schema_digest()
        {
            return Err(NativeTerminalManagerError::AuthorityChanged);
        }
        Ok((context, matched))
    }

    pub fn session(
        &self,
        owner_window_label: &str,
        session_id: &str,
    ) -> Result<Option<NativeSessionRecord>, NativeTerminalManagerError> {
        Ok(self
            .inner
            .store
            .get_session_for_window(session_id, owner_window_label)?)
    }

    /// Distinguish an in-memory stream owner, a start RPC currently in flight,
    /// and a durable-only record. Validate the durable owner before consulting
    /// RAM so corrupt or foreign records cannot fall through to another cached
    /// manager.
    pub(crate) fn session_management_ownership(
        &self,
        owner_window_label: &str,
        session_id: &str,
    ) -> Result<NativeSessionManagementOwnership, NativeTerminalManagerError> {
        let _ownership_transition = self
            .inner
            .ownership_transition
            .lock()
            .map_err(|_| NativeTerminalManagerError::Conflict)?;
        self.inner
            .store
            .get_session_for_window(session_id, owner_window_label)?
            .ok_or(NativeTerminalManagerError::NotFound)?;

        if self
            .inner
            .sessions
            .lock()
            .map_err(|_| NativeTerminalManagerError::Conflict)?
            .contains_key(session_id)
        {
            return Ok(NativeSessionManagementOwnership::Managed);
        }
        if self
            .inner
            .starting
            .lock()
            .map_err(|_| NativeTerminalManagerError::Conflict)?
            .contains(session_id)
        {
            return Ok(NativeSessionManagementOwnership::Starting);
        }
        Ok(NativeSessionManagementOwnership::None)
    }

    fn managed_sessions_for_window(
        &self,
        owner_window_label: &str,
    ) -> Result<Vec<NativeSessionRecord>, NativeTerminalManagerError> {
        let records = self
            .inner
            .store
            .list_sessions_for_window(owner_window_label)?;
        let mut managed = Vec::new();
        for record in records {
            if self.session_management_ownership(owner_window_label, &record.session_id)?
                != NativeSessionManagementOwnership::None
            {
                managed.push(record);
            }
        }
        Ok(managed)
    }

    /// Claim the durable cancel intent only when this process still owns the
    /// in-memory start or stream. `None` means the record is durable-only and
    /// must not be changed by this manager.
    fn claim_cancel_for_owned_session(
        &self,
        owner_window_label: &str,
        session_id: &str,
    ) -> Result<Option<NativeCancelClaim>, NativeTerminalManagerError> {
        let current = self
            .inner
            .store
            .get_session_for_window(session_id, owner_window_label)?
            .ok_or(NativeTerminalManagerError::NotFound)?;
        if matches!(
            current.state,
            SessionState::Closed | SessionState::Cancelled
        ) {
            return Ok(Some(NativeCancelClaim::Terminal));
        }
        if self.session_management_ownership(owner_window_label, session_id)?
            == NativeSessionManagementOwnership::None
        {
            return Ok(current
                .cancel_requested
                .then_some(NativeCancelClaim::AlreadyRequested));
        }

        let claim = self
            .inner
            .store
            .claim_cancel(session_id, owner_window_label)?;
        if claim == NativeCancelClaim::PendingAcceptance {
            let known_only_in_memory = self
                .inner
                .sessions
                .lock()
                .map_err(|_| NativeTerminalManagerError::Conflict)?
                .get(session_id)
                .is_some_and(|session| session.native_job_id.is_some());
            if known_only_in_memory {
                // Persistence failed after Jobworkerp returned an ID; do not
                // treat RAM as a second cancellation authority.
                self.inner.store.mark_start_unknown(session_id)?;
            }
        }
        Ok(Some(claim))
    }

    fn mark_owned_session_unknown(
        &self,
        owner_window_label: &str,
        session_id: &str,
    ) -> Result<(), NativeTerminalManagerError> {
        if self.session_management_ownership(owner_window_label, session_id)?
            == NativeSessionManagementOwnership::None
        {
            return Ok(());
        }
        match self.inner.store.mark_start_unknown(session_id) {
            Ok(_) => Ok(()),
            Err(NativeTerminalStoreError::Conflict) => {
                let current = self
                    .inner
                    .store
                    .get_session_for_window(session_id, owner_window_label)?
                    .ok_or(NativeTerminalManagerError::NotFound)?;
                if matches!(
                    current.state,
                    SessionState::Closed | SessionState::Cancelled
                ) {
                    Ok(())
                } else {
                    Err(NativeTerminalStoreError::Conflict.into())
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Return the already-durable session for an owner-bound preview, if any.
    /// This is deliberately read-only: a record from a previous process can be
    /// returned without claiming that its SDK or worker is currently usable.
    pub(crate) fn existing_session_for_preview(
        &self,
        owner_window_label: &str,
        preview_id: &str,
        preview_digest: &str,
    ) -> Result<Option<NativeSessionRecord>, NativeTerminalManagerError> {
        if !is_canonical_uuid(preview_id) {
            return Err(NativeTerminalManagerError::InvalidRequest);
        }
        let record = self
            .inner
            .store
            .list_sessions_for_window(owner_window_label)?
            .into_iter()
            .find(|record| record.preview_id == preview_id);
        if let Some(record) = record {
            if record.preview_digest != preview_digest {
                return Err(NativeTerminalManagerError::Conflict);
            }
            return Ok(Some(record));
        }
        Ok(None)
    }

    /// List only the specified Issue's sessions belonging to this native
    /// window. Durable records are revalidated by the private store first.
    pub(crate) fn list_sessions_for_issue(
        &self,
        owner_window_label: &str,
        issue_id: &str,
    ) -> Result<Vec<NativeSessionRecord>, NativeTerminalManagerError> {
        Ok(self
            .inner
            .store
            .list_sessions_for_window(owner_window_label)?
            .into_iter()
            .filter(|record| record.context.canonical_issue().issue_id() == issue_id)
            .collect())
    }

    pub fn output_page(
        &self,
        owner_window_label: &str,
        session_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<NativeTerminalOutputRecord>, NativeTerminalManagerError> {
        if self
            .inner
            .store
            .get_session_for_window(session_id, owner_window_label)?
            .is_none()
        {
            return Err(NativeTerminalManagerError::NotFound);
        }
        Ok(self
            .inner
            .output
            .page_after(session_id, after_sequence, limit)?)
    }

    /// Subscribe before reading the live display buffer; sequence checks reconcile the race.
    pub fn attach(
        &self,
        owner_window_label: &str,
        session_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> Result<NativeTerminalAttachment, NativeTerminalManagerError> {
        let record = self
            .inner
            .store
            .get_session_for_window(session_id, owner_window_label)?
            .ok_or(NativeTerminalManagerError::NotFound)?;
        let receiver = {
            let sessions = self
                .inner
                .sessions
                .lock()
                .map_err(|_| NativeTerminalManagerError::Conflict)?;
            match sessions.get(session_id) {
                Some(session) => session.output.subscribe(),
                None => broadcast::channel(NATIVE_OUTPUT_SUBSCRIBER_CAPACITY).1,
            }
        };
        let events = self
            .inner
            .output
            .page_after(session_id, after_sequence, limit)?;
        let next_sequence = events.last().map_or(after_sequence, |event| event.sequence);
        Ok(NativeTerminalAttachment {
            record,
            events,
            next_sequence,
            subscription: NativeTerminalSubscription {
                receiver,
                last_sequence: next_sequence,
            },
        })
    }

    pub async fn send_input(
        &self,
        owner_window_label: &str,
        session_id: &str,
        bytes: Vec<u8>,
    ) -> Result<(), NativeTerminalManagerError> {
        let record = self
            .inner
            .store
            .get_session_for_window(session_id, owner_window_label)?
            .ok_or(NativeTerminalManagerError::NotFound)?;
        if record.state != SessionState::Running || record.cancel_requested {
            return Err(NativeTerminalManagerError::SessionNotRunning);
        }
        let input = self
            .inner
            .sessions
            .lock()
            .map_err(|_| NativeTerminalManagerError::Conflict)?
            .get(session_id)
            .and_then(|session| session.input.clone())
            .ok_or(NativeTerminalManagerError::SessionNotRunning)?;
        if bytes.len() > MAX_OUTPUT_FRAME_BYTES {
            return Err(NativeTerminalManagerError::InvalidRequest);
        }
        input
            .send_input(bytes)
            .await
            .map_err(|_| NativeTerminalManagerError::Transport)
    }

    pub async fn cancel(
        &self,
        owner_window_label: &str,
        session_id: &str,
    ) -> Result<NativeSessionRecord, NativeTerminalManagerError> {
        let Some(claim) = self.claim_cancel_for_owned_session(owner_window_label, session_id)?
        else {
            return Err(NativeTerminalManagerError::SessionNotRunning);
        };
        match claim {
            NativeCancelClaim::Dispatch { job_id } => {
                if self.dispatch_cancel(job_id).await.is_err() {
                    self.mark_owned_session_unknown(owner_window_label, session_id)?;
                    return Err(NativeTerminalManagerError::Transport);
                }
            }
            // The enqueue owner claims dispatch atomically when it persists the positive job ID.
            NativeCancelClaim::PendingAcceptance => {}
            NativeCancelClaim::AlreadyRequested | NativeCancelClaim::Terminal => {}
        }
        self.inner
            .store
            .get_session_for_window(session_id, owner_window_label)?
            .ok_or(NativeTerminalManagerError::NotFound)
    }

    /// Mark interrupted sessions Unknown and return only the caller's validated durable records.
    pub fn recover_for_window(
        &self,
        owner_window_label: &str,
    ) -> Result<Vec<NativeSessionRecord>, NativeTerminalManagerError> {
        self.inner.store.mark_unresolved_unknown()?;
        Ok(self
            .inner
            .store
            .list_sessions_for_window(owner_window_label)?)
    }

    /// Request cancellation only for sessions managed by this process, then
    /// wait up to `drain_timeout` for their actual terminal stream facts. The
    /// cancellation RPC phase is separately bounded by its transport timeout.
    pub async fn shutdown_for_window(
        &self,
        owner_window_label: &str,
        drain_timeout: Duration,
    ) -> Result<Vec<NativeSessionRecord>, NativeTerminalManagerError> {
        let deadline = tokio::time::Instant::now() + drain_timeout;
        let records = self.managed_sessions_for_window(owner_window_label)?;
        let mut cancellation_jobs = Vec::new();
        for record in records {
            if matches!(record.state, SessionState::Closed | SessionState::Cancelled) {
                continue;
            }
            if let Some(NativeCancelClaim::Dispatch { job_id }) =
                self.claim_cancel_for_owned_session(owner_window_label, &record.session_id)?
            {
                cancellation_jobs.push((record.session_id, job_id));
            }
        }

        // Claim intents synchronously above, including PendingAcceptance for a
        // start RPC whose positive job ID has not arrived. Dispatch known IDs
        // concurrently so a window with several owned sessions still has one
        // bounded cancellation-RPC phase.
        let mut cancellation_tasks = tokio::task::JoinSet::new();
        let mut pending_ids = HashSet::new();
        for (session_id, job_id) in cancellation_jobs {
            pending_ids.insert(session_id.clone());
            let manager = self.clone();
            cancellation_tasks.spawn(async move {
                let result = manager.dispatch_cancel(job_id).await;
                (session_id, result)
            });
        }
        let mut failed_cancel_ids = HashSet::new();
        loop {
            match tokio::time::timeout_at(deadline, cancellation_tasks.join_next()).await {
                Ok(Some(joined)) => match joined {
                    Ok((session_id, Ok(()))) => {
                        pending_ids.remove(&session_id);
                    }
                    Ok((session_id, Err(_))) => {
                        pending_ids.remove(&session_id);
                        failed_cancel_ids.insert(session_id.clone());
                        self.mark_owned_session_unknown(owner_window_label, &session_id)?;
                    }
                    Err(_) => {
                        failed_cancel_ids.extend(pending_ids.iter().cloned());
                        cancellation_tasks.abort_all();
                        for session_id in &pending_ids {
                            self.mark_owned_session_unknown(owner_window_label, session_id)?;
                        }
                        while cancellation_tasks.join_next().await.is_some() {}
                        break;
                    }
                },
                Ok(None) => break,
                Err(_) => {
                    failed_cancel_ids.extend(pending_ids.iter().cloned());
                    cancellation_tasks.abort_all();
                    for session_id in &pending_ids {
                        self.mark_owned_session_unknown(owner_window_label, session_id)?;
                    }
                    while cancellation_tasks.join_next().await.is_some() {}
                    break;
                }
            }
        }

        loop {
            let records = self
                .inner
                .store
                .list_sessions_for_window(owner_window_label)?;
            let managed = self.managed_sessions_for_window(owner_window_label)?;
            let active = managed.iter().any(|record| {
                !matches!(record.state, SessionState::Closed | SessionState::Cancelled)
            });
            if !active {
                if failed_cancel_ids.iter().any(|session_id| {
                    records.iter().any(|record| {
                        record.session_id == *session_id
                            && !matches!(
                                record.state,
                                SessionState::Closed | SessionState::Cancelled
                            )
                    })
                }) {
                    return Err(NativeTerminalManagerError::Transport);
                }
                return Ok(records);
            }
            if tokio::time::Instant::now() >= deadline {
                for record in managed.iter().filter(|record| {
                    !matches!(record.state, SessionState::Closed | SessionState::Cancelled)
                }) {
                    self.mark_owned_session_unknown(owner_window_label, &record.session_id)?;
                }
                let records = self
                    .inner
                    .store
                    .list_sessions_for_window(owner_window_label)?;
                if failed_cancel_ids.iter().any(|session_id| {
                    records.iter().any(|record| {
                        record.session_id == *session_id
                            && !matches!(
                                record.state,
                                SessionState::Closed | SessionState::Cancelled
                            )
                    })
                }) {
                    return Err(NativeTerminalManagerError::Transport);
                }
                return Ok(records);
            }
            tokio::time::sleep(
                deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .min(Duration::from_millis(20)),
            )
            .await;
        }
    }

    async fn dispatch_cancel(&self, job_id: i64) -> Result<(), NativeTerminalManagerError> {
        tokio::time::timeout(
            NATIVE_CANCEL_RPC_TIMEOUT,
            self.inner.jobworkerp.cancel(JobId { value: job_id }),
        )
        .await
        .map_err(|_| NativeTerminalManagerError::Transport)?
        .map_err(|_| NativeTerminalManagerError::Transport)
    }

    fn drop_staged_match(&self, preview_id: &str) {
        if let Ok(mut staged) = self.inner.staged.lock()
            && let Some(session) = staged.get_mut(preview_id)
        {
            session.matched = None;
        }
    }
}

fn validate_worker_binding(
    context: &NativeSessionContext,
    matched: &NativeWorkerMatch,
) -> Result<(), NativeTerminalManagerError> {
    let binding = context
        .preview()
        .worker_binding()
        .ok_or(NativeTerminalManagerError::WorkerMismatch)?;
    let scope = matched.scope();
    let worker_repository = scope
        .repository_id()
        .ok_or(NativeTerminalManagerError::WorkerMismatch)?;
    let repository = &context.repository().identity;
    let workspace = context.workspace_mount_binding();
    let worker_name = binding.worker_name.strip_prefix("lookback.interactive.");
    if matched.using() != "run_with_client"
        || matched.worker_id().value <= 0
        || matched.runner_id().value <= 0
        || binding.worker_id != matched.worker_id().value
        || binding.runner_id != matched.runner_id().value
        || binding.settings_digest != matched.settings_digest()
        || scope.native_profile_id() != context.profile().profile_id
        || worker_repository.host != repository.host
        || worker_repository.owner != repository.organization
        || worker_repository.name != repository.repository
        || scope.canonical_host_root() != workspace.canonical_host_root
        || scope.guest_root() != workspace.guest_root
        || scope.image_digest() != context.profile().image_digest
        || scope.cpu_millis() != context.profile().resources.cpu_millis
        || scope.memory_mib() != context.profile().resources.memory_mib
        || scope.root_disk_mib() != context.profile().resources.root_disk_mib
        || scope.use_static() != context.profile().resources.use_static
        || binding.image_digest != context.profile().image_digest
        || context.preview().safe_image_digest() != context.profile().image_digest
        || context.preview().profile_id() != context.profile().profile_id
        || context.cli_profile().runtime_image_digest != context.profile().image_digest
        || context.cli_profile().cli_version != context.preview().expected_cli_version()
        || context.launch_plan().agent_kind() != context.cli_profile().kind
        || worker_name.is_none_or(|value| {
            value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        || matched
            .host_context()
            .microsandbox_sdk_home
            .as_os_str()
            .is_empty()
        || matched
            .host_context()
            .backend_config_path
            .as_os_str()
            .is_empty()
    {
        return Err(NativeTerminalManagerError::WorkerMismatch);
    }
    Ok(())
}

fn uncertain_record(mut record: NativeSessionRecord, native_job_id: i64) -> NativeSessionRecord {
    record.native_job_id = Some(native_job_id);
    record.state = SessionState::Unknown;
    record
}

fn spawn_output_pump(
    store: Arc<NativeTerminalStore>,
    output: Arc<NativeTerminalOutputStore>,
    sessions: Arc<Mutex<HashMap<String, ManagedSession>>>,
    session_id: String,
    mut stream: CodingAgentStream,
    sender: broadcast::Sender<NativeTerminalOutputRecord>,
) {
    tokio::spawn(async move {
        let mut exit_code = None;
        let mut stream_error = false;
        let mut ended_cleanly = false;
        loop {
            match stream.next_event().await {
                Ok(Some(CodingAgentEvent::Output {
                    stream: stream_name,
                    data,
                })) => {
                    if data.is_empty() {
                        continue;
                    }
                    for chunk in data.chunks(MAX_OUTPUT_FRAME_BYTES) {
                        if buffer_and_publish(
                            &output,
                            &session_id,
                            &sender,
                            NativeTerminalOutputEvent::Output {
                                stream: stream_name.clone(),
                                data: chunk.to_vec(),
                            },
                        )
                        .is_err()
                        {
                            stream_error = true;
                            let _ = store.record_terminal_observation(
                                &session_id,
                                NativeTerminalObservation {
                                    exit_code,
                                    end_received: false,
                                    stream_eof: false,
                                    stream_error: true,
                                },
                            );
                        }
                    }
                }
                Ok(Some(CodingAgentEvent::Exit {
                    exit_code: observed_exit,
                    execution_time_ms,
                    sandbox_id,
                })) => {
                    exit_code = Some(observed_exit);
                    if buffer_and_publish(
                        &output,
                        &session_id,
                        &sender,
                        NativeTerminalOutputEvent::Exit {
                            exit_code: observed_exit,
                            execution_time_ms,
                            sandbox_id,
                        },
                    )
                    .is_err()
                    {
                        stream_error = true;
                        let _ = store.record_terminal_observation(
                            &session_id,
                            NativeTerminalObservation {
                                exit_code,
                                end_received: false,
                                stream_eof: false,
                                stream_error: true,
                            },
                        );
                    }
                }
                Ok(None) => {
                    ended_cleanly = true;
                    if buffer_and_publish(
                        &output,
                        &session_id,
                        &sender,
                        NativeTerminalOutputEvent::End,
                    )
                    .is_err()
                    {
                        stream_error = true;
                    }
                    break;
                }
                Err(_) => {
                    stream_error = true;
                    break;
                }
            }
        }

        let observation = NativeTerminalObservation {
            exit_code,
            end_received: ended_cleanly,
            stream_eof: ended_cleanly,
            stream_error,
        };
        if store
            .record_terminal_observation(&session_id, observation)
            .is_err()
        {
            let _ = store.mark_start_unknown(&session_id);
        }
        if let Ok(mut sessions) = sessions.lock()
            && let Some(session) = sessions.get_mut(&session_id)
        {
            session.input = None;
        }
    });
}

fn buffer_and_publish(
    output: &NativeTerminalOutputStore,
    session_id: &str,
    sender: &broadcast::Sender<NativeTerminalOutputRecord>,
    event: NativeTerminalOutputEvent,
) -> Result<(), ()> {
    let record = output.append(session_id, event).map_err(|_| ())?;
    let _ = sender.send(record);
    Ok(())
}

impl From<NativeTerminalStoreError> for NativeTerminalManagerError {
    fn from(error: NativeTerminalStoreError) -> Self {
        match error {
            NativeTerminalStoreError::OwnerMismatch => Self::OwnerMismatch,
            NativeTerminalStoreError::NotFound => Self::NotFound,
            NativeTerminalStoreError::Conflict => Self::Conflict,
            error => Self::Store(error),
        }
    }
}

impl From<NativeTerminalOutputError> for NativeTerminalManagerError {
    fn from(error: NativeTerminalOutputError) -> Self {
        Self::Output(error)
    }
}
