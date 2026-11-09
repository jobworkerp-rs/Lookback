use crate::{
    error::{AppError, AppResult},
    grpc::{
        issue_tracker::IssueTrackerClientError,
        proto::issue_tracker::{manager::v1 as manager, v1},
    },
};

pub(super) fn parse_revision(value: &str) -> AppResult<u64> {
    value
        .parse()
        .map_err(|_| AppError::Config("Invalid revision".into()))
}

pub(super) fn require_rationale(value: Option<&str>) -> AppResult<&str> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::Config("This decision requires a reason".into()))
}

pub(super) fn map_client_error(error: IssueTrackerClientError) -> AppError {
    AppError::IssueTrackerServer(error.to_string())
}

pub(super) fn human_actor(reason: Option<String>) -> manager::ActorAudit {
    manager::ActorAudit {
        actor_id: "lookback-user".into(),
        actor_kind: v1::ActorKind::Human as i32,
        display_name: Some("Lookback user".into()),
        reason,
        ..Default::default()
    }
}

pub(super) fn durable_operation_state_name(state: manager::DurableOperationState) -> String {
    match state {
        manager::DurableOperationState::Received => "RECEIVED",
        manager::DurableOperationState::Pending => "PENDING",
        manager::DurableOperationState::Committed => "COMMITTED",
        manager::DurableOperationState::Failed => "FAILED",
        manager::DurableOperationState::Unconfirmed => "UNCONFIRMED",
        manager::DurableOperationState::ManualResolution => "MANUAL_RESOLUTION",
        manager::DurableOperationState::Arbitrated => "ARBITRATED",
        manager::DurableOperationState::Unspecified => "UNKNOWN",
    }
    .into()
}
