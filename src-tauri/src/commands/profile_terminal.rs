//! Window-owned authentication shells, independent of Issue execution.
use crate::{
    commands::{
        AppState,
        coding_agent::{
            PrepareCodingAgentProfileHomeRequest, settings_admission, unresolved_profile_ids,
        },
    },
    error::{AppError, AppResult},
    native_terminal::{
        profile_settings::{ProfileSettingsError, ProfileSettingsSession},
        wire::{
            NativeTerminalInputRequest, NativeTerminalOutputPageWire, NativeTerminalOutputRequest,
            NativeTerminalSessionRequest,
        },
    },
};
use tauri::{State, WebviewWindow};

fn map_error(error: ProfileSettingsError) -> AppError {
    AppError::Config(error.to_string())
}

#[tauri::command]
pub async fn coding_agent_profile_terminal_start(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: PrepareCodingAgentProfileHomeRequest,
) -> AppResult<ProfileSettingsSession> {
    let admission = settings_admission(&state).await?;
    let unresolved = unresolved_profile_ids(&state)?;
    state
        .native_terminal_registry
        .start_profile_settings_terminal(
            window.label(),
            request.expected_revision,
            &request.profile_id,
            &unresolved,
            &admission,
        )
        .await
        .map_err(|error| AppError::Config(error.user_message().into()))
}

#[tauri::command]
pub fn coding_agent_profile_terminal_list(
    state: State<'_, AppState>,
    window: WebviewWindow,
) -> Vec<ProfileSettingsSession> {
    state
        .native_terminal_registry
        .profile_settings_terminals
        .list(window.label())
}

#[tauri::command]
pub fn coding_agent_profile_terminal_session(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: NativeTerminalSessionRequest,
) -> AppResult<ProfileSettingsSession> {
    state
        .native_terminal_registry
        .profile_settings_terminals
        .session(window.label(), &request.session_id)
        .map_err(map_error)
}

#[tauri::command]
pub fn coding_agent_profile_terminal_output(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: NativeTerminalOutputRequest,
) -> AppResult<NativeTerminalOutputPageWire> {
    let after = crate::native_terminal::registry::parse_sequence(&request.after_sequence)
        .map_err(|_| AppError::Config("profile terminal sequence is invalid".into()))?;
    state
        .native_terminal_registry
        .profile_settings_terminals
        .output(window.label(), &request.session_id, after)
        .map_err(map_error)
}

#[tauri::command]
pub async fn coding_agent_profile_terminal_send_input(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: NativeTerminalInputRequest,
) -> AppResult<()> {
    let _admission = settings_admission(&state).await?;
    state
        .native_terminal_registry
        .profile_settings_terminals
        .send_input(window.label(), &request.session_id, request.bytes)
        .await
        .map_err(map_error)
}

#[tauri::command]
pub async fn coding_agent_profile_terminal_finish_input(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: NativeTerminalSessionRequest,
) -> AppResult<()> {
    let _admission = settings_admission(&state).await?;
    state
        .native_terminal_registry
        .profile_settings_terminals
        .finish_input(window.label(), &request.session_id)
        .await
        .map_err(map_error)
}

#[tauri::command]
pub async fn coding_agent_profile_terminal_cancel(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: NativeTerminalSessionRequest,
) -> AppResult<()> {
    state
        .native_terminal_registry
        .profile_settings_terminals
        .cancel(window.label(), &request.session_id)
        .await
        .map_err(map_error)
}
