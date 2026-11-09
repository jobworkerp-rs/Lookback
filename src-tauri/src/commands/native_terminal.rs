//! Tauri boundary for window-owned native interactive terminal sessions.

use std::sync::Arc;

use async_trait::async_trait;
use tauri::{Emitter, EventTarget, State, WebviewWindow};

use crate::{
    commands::AppState,
    error::{AppError, AppResult},
    native_terminal::{
        NativeTerminalOutputSink,
        registry::{NativeRuntimeRegistryError, NativeTerminalAttachResult},
        wire::{
            NativeTerminalAttachRequest, NativeTerminalAttachmentWire, NativeTerminalDetachRequest,
            NativeTerminalInputDeliveryWire, NativeTerminalInputRequest,
            NativeTerminalOutputEventWire, NativeTerminalOutputPageWire,
            NativeTerminalOutputRequest, NativeTerminalPrepareRequest, NativeTerminalPreviewWire,
            NativeTerminalSessionRequest, NativeTerminalSessionWire, NativeTerminalStartRequest,
            NativeTerminalStatusWire,
        },
    },
};

const NATIVE_TERMINAL_OUTPUT_EVENT: &str = "native-terminal-output";

struct WindowNativeTerminalOutputSink {
    owner_window_label: String,
    window: WebviewWindow,
}

fn output_event_target(owner_window_label: &str) -> EventTarget {
    EventTarget::webview_window(owner_window_label)
}

#[async_trait]
impl NativeTerminalOutputSink for WindowNativeTerminalOutputSink {
    async fn emit(&self, event: NativeTerminalOutputEventWire) -> Result<(), ()> {
        self.window
            .emit_to(
                output_event_target(&self.owner_window_label),
                NATIVE_TERMINAL_OUTPUT_EVENT,
                event,
            )
            .map_err(|_| ())
    }
}

fn map_registry_error(error: NativeRuntimeRegistryError) -> AppError {
    AppError::Config(error.user_message().to_owned())
}

#[tauri::command]
pub async fn native_terminal_status(
    state: State<'_, AppState>,
    issue_id: String,
) -> AppResult<NativeTerminalStatusWire> {
    state
        .native_terminal_registry
        .status(&issue_id)
        .await
        .map_err(map_registry_error)
}

#[tauri::command]
pub async fn native_terminal_prepare(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: NativeTerminalPrepareRequest,
) -> AppResult<NativeTerminalPreviewWire> {
    state.ensure_coding_agent_accepting()?;
    let owner_window_label = window.label().to_owned();
    state
        .native_terminal_registry
        .prepare(&owner_window_label, request)
        .await
        .map_err(map_registry_error)
}

#[tauri::command]
pub async fn native_terminal_start(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: NativeTerminalStartRequest,
) -> AppResult<NativeTerminalSessionWire> {
    state.ensure_coding_agent_accepting()?;
    let owner_window_label = window.label().to_owned();
    state
        .native_terminal_registry
        .start(&owner_window_label, request)
        .await
        .map_err(map_registry_error)
}

#[tauri::command]
pub async fn native_terminal_list(
    state: State<'_, AppState>,
    window: WebviewWindow,
    issue_id: String,
) -> AppResult<Vec<NativeTerminalSessionWire>> {
    let owner_window_label = window.label().to_owned();
    state
        .native_terminal_registry
        .list(&owner_window_label, &issue_id)
        .await
        .map_err(map_registry_error)
}

#[tauri::command]
pub async fn native_terminal_attach(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: NativeTerminalAttachRequest,
) -> AppResult<NativeTerminalAttachmentWire> {
    let owner_window_label = window.label().to_owned();
    let sink = Arc::new(WindowNativeTerminalOutputSink {
        owner_window_label: owner_window_label.clone(),
        window,
    });
    let result: NativeTerminalAttachResult = state
        .native_terminal_registry
        .attach(&owner_window_label, request, sink)
        .await
        .map_err(map_registry_error)?;
    Ok(result.attachment)
}

#[tauri::command]
pub async fn native_terminal_output(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: NativeTerminalOutputRequest,
) -> AppResult<NativeTerminalOutputPageWire> {
    let owner_window_label = window.label().to_owned();
    state
        .native_terminal_registry
        .output_page(&owner_window_label, request)
        .await
        .map_err(map_registry_error)
}

#[tauri::command]
pub async fn native_terminal_send_input(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: NativeTerminalInputRequest,
) -> AppResult<NativeTerminalInputDeliveryWire> {
    // Serialize stdin admission with shutdown/settings transitions; this operation
    // does not acquire the registry's admission lock internally.
    let _admission = state.native_terminal_admission.lock().await;
    if state.ensure_coding_agent_accepting().is_err() {
        return Ok(NativeTerminalInputDeliveryWire::Unavailable);
    }
    let owner_window_label = window.label().to_owned();
    state
        .native_terminal_registry
        .send_input(&owner_window_label, request)
        .await
        .map_err(map_registry_error)
}

#[tauri::command]
pub async fn native_terminal_cancel(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: NativeTerminalSessionRequest,
) -> AppResult<NativeTerminalSessionWire> {
    let owner_window_label = window.label().to_owned();
    state
        .native_terminal_registry
        .cancel(&owner_window_label, &request.session_id)
        .await
        .map_err(map_registry_error)
}

#[tauri::command]
pub async fn native_terminal_detach(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: NativeTerminalDetachRequest,
) -> AppResult<()> {
    let owner_window_label = window.label().to_owned();
    state
        .native_terminal_registry
        .detach(&owner_window_label, request)
        .await
        .map_err(map_registry_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_errors_are_reduced_to_fixed_renderer_safe_messages() {
        let error = map_registry_error(NativeRuntimeRegistryError::SourceUnavailable);

        assert_eq!(
            error.to_string(),
            "config: The managed native runtime SDK source is unavailable or changed. Restart the local worker and try again."
        );
        assert!(!error.to_string().contains("path"));
        assert!(!error.to_string().contains("credential"));
    }

    #[test]
    fn output_is_targeted_to_the_owning_webview_window_only() {
        assert_eq!(
            output_event_target("main"),
            EventTarget::WebviewWindow {
                label: "main".to_owned()
            }
        );
    }
}
