//! Narrow JSON DTOs for the renderer-facing native terminal commands.
//!
//! Native session context and authority types intentionally do not implement
//! renderer serialization. These projections contain only the fields in the
//! stable frontend API contract.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    NativeSessionContext, NativeSessionRecord, NativeTerminalOutputEvent,
    NativeTerminalOutputRecord, SessionState,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalProfileWire {
    pub profile_id: String,
    pub label: String,
    pub image_uri: String,
    pub expected_cli_version: String,
    pub allowed_base_branches: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalStatusWire {
    pub available: bool,
    pub blockers: Vec<String>,
    pub profiles: Vec<NativeTerminalProfileWire>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalPrepareRequest {
    pub issue_id: String,
    pub profile_id: String,
    pub base_branch: String,
    pub prompt: Option<String>,
}

impl std::fmt::Debug for NativeTerminalPrepareRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeTerminalPrepareRequest")
            .field("issue_id", &self.issue_id)
            .field("profile_id", &self.profile_id)
            .field("base_branch", &self.base_branch)
            .field("prompt", &self.prompt.as_ref().map(|prompt| prompt.len()))
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalPrepareArgs {
    pub request: NativeTerminalPrepareRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalStartRequest {
    pub preview_id: String,
    pub preview_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalStartArgs {
    pub request: NativeTerminalStartRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalSessionRequest {
    pub session_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalSessionArgs {
    pub request: NativeTerminalSessionRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalAttachRequest {
    pub session_id: String,
    pub after_sequence: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalAttachArgs {
    pub request: NativeTerminalAttachRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalOutputRequest {
    pub session_id: String,
    pub after_sequence: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalOutputArgs {
    pub request: NativeTerminalOutputRequest,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalInputRequest {
    pub session_id: String,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for NativeTerminalInputRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeTerminalInputRequest")
            .field("session_id", &self.session_id)
            .field("byte_count", &self.bytes.len())
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalInputArgs {
    pub request: NativeTerminalInputRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalDetachRequest {
    pub session_id: String,
    pub subscription_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalDetachArgs {
    pub request: NativeTerminalDetachRequest,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeTerminalSessionStateWire {
    Starting,
    Running,
    Closed,
    Cancelled,
    Unknown,
}

impl From<SessionState> for NativeTerminalSessionStateWire {
    fn from(value: SessionState) -> Self {
        match value {
            SessionState::Starting => Self::Starting,
            SessionState::Running => Self::Running,
            SessionState::Closed => Self::Closed,
            SessionState::Cancelled => Self::Cancelled,
            SessionState::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalSessionWire {
    pub session_id: String,
    pub preview_id: String,
    pub profile_id: String,
    pub issue_id: String,
    pub state: NativeTerminalSessionStateWire,
    pub job_id: Option<String>,
    pub exit_code: Option<i32>,
    pub end_received: bool,
    pub stream_eof: bool,
    pub stream_error: Option<String>,
    pub cancel_requested: bool,
}

impl NativeTerminalSessionWire {
    pub fn from_record(record: &NativeSessionRecord) -> Result<Self, NativeTerminalWireError> {
        let job_id = match record.native_job_id {
            Some(id) if id > 0 => Some(id.to_string()),
            Some(_) => return Err(NativeTerminalWireError::InvalidRecord),
            None => None,
        };
        Ok(Self {
            session_id: record.session_id.clone(),
            preview_id: record.preview_id.clone(),
            profile_id: record.profile_id.clone(),
            issue_id: record.context.canonical_issue().issue_id().to_owned(),
            state: record.state.into(),
            job_id,
            exit_code: record.exit_code,
            end_received: record.end_received,
            stream_eof: record.stream_eof,
            stream_error: record.stream_error.then(|| "stream_error".to_owned()),
            cancel_requested: record.cancel_requested,
        })
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalPreviewWire {
    pub preview_id: String,
    pub preview_digest: String,
    pub session_id: String,
    pub profile_id: String,
    pub expected_settings_revision: String,
    pub expected_issue_revision: String,
    pub repository: NativeTerminalRepositoryWire,
    pub base_branch: String,
    pub base_commit: String,
    pub command: String,
    pub args: Vec<String>,
    pub working_directory: String,
    pub initial_prompt: String,
    pub expected_cli_version: String,
    pub safe_image_digest: String,
    pub runtime_observed: bool,
}

impl std::fmt::Debug for NativeTerminalPreviewWire {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeTerminalPreviewWire")
            .field("preview_id", &self.preview_id)
            .field("preview_digest", &self.preview_digest)
            .field("session_id", &self.session_id)
            .field("profile_id", &self.profile_id)
            .field(
                "expected_settings_revision",
                &self.expected_settings_revision,
            )
            .field("expected_issue_revision", &self.expected_issue_revision)
            .field("repository", &self.repository)
            .field("base_branch", &self.base_branch)
            .field("command", &self.command)
            .field("argument_count", &self.args.len())
            .field("initial_prompt_bytes", &self.initial_prompt.len())
            .field("expected_cli_version", &self.expected_cli_version)
            .field("safe_image_digest", &self.safe_image_digest)
            .field("runtime_observed", &self.runtime_observed)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalRepositoryWire {
    pub host: String,
    pub organization: String,
    pub repository: String,
}

impl NativeTerminalPreviewWire {
    pub fn from_context(
        context: &NativeSessionContext,
        runtime_observed: bool,
    ) -> Result<Self, NativeTerminalWireError> {
        const MAX_REVISION: u64 = i64::MAX as u64;
        let preview = context.preview();
        if preview.expected_settings_revision() > MAX_REVISION
            || preview.expected_issue_revision() > MAX_REVISION
        {
            return Err(NativeTerminalWireError::InvalidRecord);
        }
        let repository = &context.repository().identity;
        let launch = context.launch_plan();
        Ok(Self {
            preview_id: preview.preview_id().to_owned(),
            preview_digest: preview.preview_digest().to_owned(),
            session_id: context.session_id().to_owned(),
            profile_id: context.profile().profile_id.clone(),
            expected_settings_revision: preview.expected_settings_revision().to_string(),
            expected_issue_revision: preview.expected_issue_revision().to_string(),
            repository: NativeTerminalRepositoryWire {
                host: repository.host.clone(),
                organization: repository.organization.clone(),
                repository: repository.repository.clone(),
            },
            base_branch: context.base_resolution_receipt().base_branch.clone(),
            base_commit: context.base_resolution_receipt().base_commit.clone(),
            command: launch.command().to_owned(),
            args: launch.args().to_vec(),
            working_directory: launch.working_directory().to_owned(),
            initial_prompt: launch.initial_prompt().text().to_owned(),
            expected_cli_version: launch.expected_cli_version().to_owned(),
            safe_image_digest: launch.image_digest().to_owned(),
            runtime_observed,
        })
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum NativeTerminalEventWire {
    Output {
        stream: String,
        data: Vec<u8>,
    },
    Exit {
        exit_code: i32,
        execution_time_ms: String,
        sandbox_id: String,
    },
    End,
}

impl std::fmt::Debug for NativeTerminalEventWire {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Output { stream, data } => formatter
                .debug_struct("Output")
                .field("stream", stream)
                .field("byte_count", &data.len())
                .finish(),
            Self::Exit {
                exit_code,
                execution_time_ms,
                ..
            } => formatter
                .debug_struct("Exit")
                .field("exit_code", exit_code)
                .field("execution_time_ms", execution_time_ms)
                .field("sandbox_id", &"<private runtime identifier>")
                .finish(),
            Self::End => formatter.write_str("End"),
        }
    }
}

impl From<&NativeTerminalOutputEvent> for NativeTerminalEventWire {
    fn from(value: &NativeTerminalOutputEvent) -> Self {
        match value {
            NativeTerminalOutputEvent::Output { stream, data } => Self::Output {
                stream: stream.clone(),
                data: data.clone(),
            },
            NativeTerminalOutputEvent::Exit {
                exit_code,
                execution_time_ms,
                sandbox_id,
            } => Self::Exit {
                exit_code: *exit_code,
                execution_time_ms: execution_time_ms.to_string(),
                sandbox_id: sandbox_id.clone(),
            },
            NativeTerminalOutputEvent::End => Self::End,
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalOutputRecordWire {
    pub session_id: String,
    pub sequence: String,
    pub event: NativeTerminalEventWire,
}

impl std::fmt::Debug for NativeTerminalOutputRecordWire {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeTerminalOutputRecordWire")
            .field("session_id", &self.session_id)
            .field("sequence", &self.sequence)
            .field("event", &self.event)
            .finish()
    }
}

impl NativeTerminalOutputRecordWire {
    pub fn from_record(
        session_id: &str,
        record: &NativeTerminalOutputRecord,
    ) -> Result<Self, NativeTerminalWireError> {
        if session_id.is_empty() || record.sequence == 0 {
            return Err(NativeTerminalWireError::InvalidRecord);
        }
        Ok(Self {
            session_id: session_id.to_owned(),
            sequence: record.sequence.to_string(),
            event: (&record.event).into(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalOutputEventWire {
    pub subscription_id: String,
    pub record: NativeTerminalOutputRecordWire,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalOutputPageWire {
    pub events: Vec<NativeTerminalOutputRecordWire>,
    pub next_sequence: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalAttachmentWire {
    pub session: NativeTerminalSessionWire,
    pub events: Vec<NativeTerminalOutputRecordWire>,
    pub next_sequence: String,
    pub subscription_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NativeTerminalInputDeliveryWire {
    Accepted,
    Unknown,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativeTerminalWireError {
    #[error("native terminal record cannot be represented by the renderer wire contract")]
    InvalidRecord,
}
