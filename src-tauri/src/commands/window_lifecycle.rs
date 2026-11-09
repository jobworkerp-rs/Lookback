use serde::Serialize;
use tauri::{AppHandle, Manager};

use super::{AppState, emit_event};
use crate::window_lifecycle::{
    CloseActionResult, CloseChoice, CloseChoiceRequest, NativeFallbackLocale, TrayAvailability,
};

pub const CLOSE_REQUESTED_EVENT: &str = "window-lifecycle://close-requested";
pub const QUIT_FAILED_EVENT: &str = "window-lifecycle://quit-failed";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseChoiceBootstrap {
    pub pending_request: Option<CloseChoiceRequest>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuitFailedPayload {
    native_fallback: bool,
}

#[tauri::command]
pub fn register_close_choice_ui(
    state: tauri::State<'_, AppState>,
    locale: String,
) -> CloseChoiceBootstrap {
    CloseChoiceBootstrap {
        pending_request: state.window_lifecycle.register_ui(&locale),
    }
}

#[tauri::command]
pub fn sync_close_choice_locale(state: tauri::State<'_, AppState>, locale: String) {
    state.window_lifecycle.sync_ui_locale(&locale);
}

#[tauri::command]
pub fn acknowledge_close_choice_request(
    state: tauri::State<'_, AppState>,
    request_id: u64,
) -> bool {
    state.window_lifecycle.acknowledge_close_request(request_id)
}

#[tauri::command]
pub fn choose_close_action(
    state: tauri::State<'_, AppState>,
    app: AppHandle,
    request_id: u64,
    choice: CloseChoice,
) -> CloseActionResult {
    let availability = if choice == CloseChoice::Background {
        state.coding_agent_tray_availability()
    } else {
        TrayAvailability::Unavailable
    };
    let result = state
        .window_lifecycle
        .select_close_action(request_id, choice, availability);
    if result != CloseActionResult::Accepted {
        return result;
    }

    match choice {
        CloseChoice::Cancel => CloseActionResult::Accepted,
        CloseChoice::Quit => {
            app.exit(0);
            CloseActionResult::Accepted
        }
        CloseChoice::Background => {
            let Some(request) = state.window_lifecycle.pending_close_request() else {
                return CloseActionResult::StaleRequest;
            };
            if request.request_id != request_id {
                return CloseActionResult::StaleRequest;
            }
            let latest_availability = state.coding_agent_tray_availability();
            if latest_availability != TrayAvailability::VerifiedRestorable {
                let reason = match latest_availability {
                    TrayAvailability::Unverified => {
                        crate::window_lifecycle::BackgroundUnavailableReason::Unverified
                    }
                    TrayAvailability::Unavailable => {
                        crate::window_lifecycle::BackgroundUnavailableReason::Unavailable
                    }
                    TrayAvailability::VerifiedRestorable => return CloseActionResult::InvalidState,
                };
                state
                    .window_lifecycle
                    .complete_background_action(request_id, Err(reason));
                if let Some(updated) = state.window_lifecycle.pending_close_request() {
                    notify_close_request(&app, updated);
                }
                return CloseActionResult::BackgroundUnavailable;
            }
            let hidden = app
                .get_webview_window(&request.window_label)
                .is_some_and(|window| window.hide().is_ok());
            if !hidden {
                restore_lifecycle_window(&app, &request.window_label);
                state.window_lifecycle.complete_background_action(
                    request_id,
                    Err(crate::window_lifecycle::BackgroundUnavailableReason::HideFailed),
                );
                if let Some(updated) = state.window_lifecycle.pending_close_request() {
                    notify_close_request(&app, updated);
                }
                return CloseActionResult::HideFailed;
            }
            state
                .window_lifecycle
                .complete_background_action(request_id, Ok(()));
            CloseActionResult::Accepted
        }
    }
}

fn restore_lifecycle_window(app: &AppHandle, label: &str) {
    if let Some(window) = app.get_webview_window(label) {
        let adapter = crate::TauriWindowRestoreAdapter { window };
        if !crate::window_lifecycle::restore_window(&adapter) {
            tracing::warn!(
                window = label,
                "the close-choice window could not be restored"
            );
        }
    }
}

pub(crate) fn schedule_native_fallback(app: AppHandle, request_id: u64) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        let locale = {
            let Some(state) = app.try_state::<AppState>() else {
                return;
            };
            state.window_lifecycle.begin_native_fallback(request_id)
        };
        let Some(locale) = locale else {
            return;
        };
        show_native_quit_dialog(app, request_id, locale);
    });
}

fn show_native_quit_dialog(app: AppHandle, request_id: u64, locale: NativeFallbackLocale) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

    let (title, message, quit, cancel) = match locale {
        NativeFallbackLocale::Japanese => (
            "Lookback を終了しますか？",
            "アプリを終了すると、実行中のサービスも安全に停止します。",
            "終了",
            "キャンセル",
        ),
        NativeFallbackLocale::English => (
            "Quit Lookback?",
            "Quitting Lookback will safely stop its running services.",
            "Quit",
            "Cancel",
        ),
        NativeFallbackLocale::Bilingual => (
            "Lookback を終了しますか？ / Quit Lookback?",
            "アプリを終了すると、実行中のサービスも安全に停止します。\nQuitting Lookback will safely stop its running services.",
            "終了 / Quit",
            "キャンセル / Cancel",
        ),
    };
    app.dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            quit.to_owned(),
            cancel.to_owned(),
        ))
        .show(move |confirmed| {
            let accepted = app.try_state::<AppState>().is_some_and(|state| {
                state
                    .window_lifecycle
                    .resolve_native_fallback(request_id, confirmed)
            });
            if accepted && confirmed {
                app.exit(0);
            }
        });
}

pub(crate) fn exit_request_failed(
    app: &AppHandle,
    native_fallback_locale: Option<NativeFallbackLocale>,
) {
    let last_ui_locale = app
        .try_state::<AppState>()
        .map(|state| state.window_lifecycle.last_ui_locale())
        .unwrap_or(NativeFallbackLocale::Bilingual);
    let restore_window = app
        .get_webview_window("main")
        .or_else(|| app.webview_windows().into_values().next());
    let restore_adapter = restore_window.map(|window| crate::TauriWindowRestoreAdapter { window });
    let retry_plan = crate::window_lifecycle::plan_exit_retry(
        restore_adapter
            .as_ref()
            .map(|adapter| adapter as &dyn crate::window_lifecycle::WindowRestoreAdapter),
        native_fallback_locale,
        last_ui_locale,
    );
    emit_event(
        app,
        QUIT_FAILED_EVENT,
        QuitFailedPayload {
            native_fallback: retry_plan.native_dialog,
        },
    );
    if let Some(locale) = retry_plan.locale {
        show_native_retry_dialog(app.clone(), locale);
    }
}

pub(crate) fn notify_close_request(app: &AppHandle, request: CloseChoiceRequest) {
    emit_event(app, CLOSE_REQUESTED_EVENT, request);
}

fn show_native_retry_dialog(app: AppHandle, locale: NativeFallbackLocale) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

    let (title, message, retry, cancel) = match locale {
        NativeFallbackLocale::Japanese => (
            "Lookback を終了できませんでした",
            "実行中のサービスを安全に停止できませんでした。もう一度終了しますか？",
            "再試行",
            "キャンセル",
        ),
        NativeFallbackLocale::English => (
            "Lookback could not quit",
            "Lookback could not stop its services safely. Would you like to try quitting again?",
            "Retry",
            "Cancel",
        ),
        NativeFallbackLocale::Bilingual => (
            "Lookback を終了できませんでした / Lookback could not quit",
            "実行中のサービスを安全に停止できませんでした。もう一度終了しますか？\nLookback could not stop its services safely. Retry quitting?",
            "再試行 / Retry",
            "キャンセル / Cancel",
        ),
    };
    app.dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Error)
        .buttons(MessageDialogButtons::OkCancelCustom(
            retry.to_owned(),
            cancel.to_owned(),
        ))
        .show(move |confirmed| {
            let accepted = confirmed
                && app.try_state::<AppState>().is_some_and(|state| {
                    state.window_lifecycle.request_native_fallback_quit(locale)
                });
            if accepted {
                app.exit(0);
            }
        });
}
