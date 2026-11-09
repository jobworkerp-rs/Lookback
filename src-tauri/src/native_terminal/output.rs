use super::is_canonical_uuid;
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Mutex,
};
use thiserror::Error;

pub(crate) const MAX_OUTPUT_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_NATIVE_OUTPUT_PAGE_EVENTS: usize = 16;
const MAX_BUFFERED_BYTES: usize = 1024 * 1024;
const MAX_BUFFERED_EVENTS: usize = 512;
const MAX_BUFFERED_SESSIONS: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeTerminalOutputEvent {
    Output {
        stream: String,
        data: Vec<u8>,
    },
    Exit {
        exit_code: i32,
        execution_time_ms: u64,
        sandbox_id: String,
    },
    End,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeTerminalOutputRecord {
    pub sequence: u64,
    pub event: NativeTerminalOutputEvent,
}

#[derive(Default)]
struct SessionBuffer {
    records: VecDeque<NativeTerminalOutputRecord>,
    bytes: usize,
    sequence: u64,
    exited: bool,
    ended: bool,
}

#[derive(Default)]
struct DisplayBuffers {
    sessions: HashMap<String, SessionBuffer>,
    order: VecDeque<String>,
}

/// Bounded live display buffering. Agent homes own their session logs.
pub struct NativeTerminalOutputStore {
    buffers: Mutex<DisplayBuffers>,
    location: PathBuf,
}

impl std::fmt::Debug for NativeTerminalOutputStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeTerminalOutputStore")
            .finish_non_exhaustive()
    }
}

impl NativeTerminalOutputStore {
    /// The legacy location is diagnostic only; no transcript is opened or written.
    pub fn open(location: &Path) -> Result<Self, NativeTerminalOutputError> {
        Ok(Self {
            buffers: Mutex::new(DisplayBuffers::default()),
            location: location.to_owned(),
        })
    }

    pub fn database_path(&self) -> &Path {
        &self.location
    }

    pub fn ensure_disjoint_from_workspace(
        &self,
        _workspace: &Path,
    ) -> Result<(), NativeTerminalOutputError> {
        Ok(())
    }

    pub fn append(
        &self,
        session_id: &str,
        event: NativeTerminalOutputEvent,
    ) -> Result<NativeTerminalOutputRecord, NativeTerminalOutputError> {
        validate_session_id(session_id)?;
        validate_event(&event)?;
        let mut buffers = self
            .buffers
            .lock()
            .map_err(|_| NativeTerminalOutputError::Unavailable)?;
        if !buffers.sessions.contains_key(session_id) {
            if buffers.sessions.len() >= MAX_BUFFERED_SESSIONS
                && let Some(index) = buffers.order.iter().position(|id| {
                    buffers
                        .sessions
                        .get(id)
                        .is_some_and(|session| session.ended)
                })
                && let Some(ended) = buffers.order.remove(index)
            {
                buffers.sessions.remove(&ended);
            }
            buffers.order.push_back(session_id.to_owned());
        }
        let session = buffers.sessions.entry(session_id.to_owned()).or_default();
        if session.ended || (session.exited && !matches!(event, NativeTerminalOutputEvent::End)) {
            return Err(NativeTerminalOutputError::Conflict);
        }
        if matches!(event, NativeTerminalOutputEvent::End) && !session.exited {
            return Err(NativeTerminalOutputError::InvalidEvent);
        }
        session.sequence = session
            .sequence
            .checked_add(1)
            .ok_or(NativeTerminalOutputError::CapacityExceeded)?;
        session.exited |= matches!(event, NativeTerminalOutputEvent::Exit { .. });
        session.ended |= matches!(event, NativeTerminalOutputEvent::End);
        let record = NativeTerminalOutputRecord {
            sequence: session.sequence,
            event,
        };
        session.bytes += event_payload_bytes(&record.event);
        session.records.push_back(record.clone());
        while session.bytes > MAX_BUFFERED_BYTES || session.records.len() > MAX_BUFFERED_EVENTS {
            if let Some(oldest) = session.records.pop_front() {
                session.bytes -= event_payload_bytes(&oldest.event);
            }
        }
        let mut total_bytes = buffers
            .sessions
            .values()
            .map(|session| session.bytes)
            .sum::<usize>();
        while total_bytes > MAX_BUFFERED_BYTES * MAX_BUFFERED_SESSIONS {
            let Some(oldest) = buffers
                .order
                .iter()
                .find(|id| {
                    buffers
                        .sessions
                        .get(*id)
                        .is_some_and(|session| session.bytes > 0)
                })
                .cloned()
            else {
                break;
            };
            if let Some(session) = buffers.sessions.get_mut(&oldest) {
                total_bytes -= session.bytes;
                session.bytes = 0;
                session.records.clear();
            }
        }
        Ok(record)
    }

    pub fn page_after(
        &self,
        session_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<NativeTerminalOutputRecord>, NativeTerminalOutputError> {
        validate_session_id(session_id)?;
        if limit == 0 || limit > MAX_NATIVE_OUTPUT_PAGE_EVENTS {
            return Err(NativeTerminalOutputError::InvalidPage);
        }
        let buffers = self
            .buffers
            .lock()
            .map_err(|_| NativeTerminalOutputError::Unavailable)?;
        let Some(session) = buffers.sessions.get(session_id) else {
            return Ok(Vec::new());
        };
        if (session.records.is_empty() && after_sequence < session.sequence)
            || session
                .records
                .front()
                .is_some_and(|record| after_sequence < record.sequence.saturating_sub(1))
        {
            return Err(NativeTerminalOutputError::Unavailable);
        }
        Ok(session
            .records
            .iter()
            .filter(|record| record.sequence > after_sequence)
            .take(limit)
            .cloned()
            .collect())
    }
}

#[derive(Debug, Error)]
pub enum NativeTerminalOutputError {
    #[error("native terminal display event is invalid")]
    InvalidEvent,
    #[error("native terminal display event conflicts with the terminal state")]
    Conflict,
    #[error("native terminal display event exceeds its frame bound")]
    EventTooLarge,
    #[error("native terminal display page request is invalid")]
    InvalidPage,
    #[error("live terminal display buffer is unavailable")]
    Unavailable,
    #[error("native terminal display sequence is exhausted")]
    CapacityExceeded,
}

fn validate_session_id(id: &str) -> Result<(), NativeTerminalOutputError> {
    if is_canonical_uuid(id) {
        Ok(())
    } else {
        Err(NativeTerminalOutputError::InvalidEvent)
    }
}

fn validate_event(event: &NativeTerminalOutputEvent) -> Result<(), NativeTerminalOutputError> {
    match event {
        NativeTerminalOutputEvent::Output { stream, data } => {
            if stream.is_empty() || stream.len() > 64 || stream.chars().any(char::is_control) {
                return Err(NativeTerminalOutputError::InvalidEvent);
            }
            if data.len() > MAX_OUTPUT_FRAME_BYTES {
                return Err(NativeTerminalOutputError::EventTooLarge);
            }
        }
        NativeTerminalOutputEvent::Exit { sandbox_id, .. }
            if sandbox_id.len() > 1024 || sandbox_id.chars().any(char::is_control) =>
        {
            return Err(NativeTerminalOutputError::InvalidEvent);
        }
        _ => {}
    }
    Ok(())
}

fn event_payload_bytes(event: &NativeTerminalOutputEvent) -> usize {
    match event {
        NativeTerminalOutputEvent::Output { stream, data } => stream.len() + data.len(),
        NativeTerminalOutputEvent::Exit { sandbox_id, .. } => sandbox_id.len(),
        NativeTerminalOutputEvent::End => 0,
    }
}
