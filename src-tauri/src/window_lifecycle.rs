use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayAvailability {
    VerifiedRestorable,
    Unverified,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundUnavailableReason {
    Unverified,
    Unavailable,
    HideFailed,
}

impl TrayAvailability {
    fn unavailable_reason(self) -> Option<BackgroundUnavailableReason> {
        match self {
            Self::VerifiedRestorable => None,
            Self::Unverified => Some(BackgroundUnavailableReason::Unverified),
            Self::Unavailable => Some(BackgroundUnavailableReason::Unavailable),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseChoice {
    Quit,
    Background,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CloseActionResult {
    Accepted,
    BackgroundUnavailable,
    HideFailed,
    StaleRequest,
    InvalidState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseChoiceRequest {
    pub request_id: u64,
    pub window_label: String,
    pub background_available: bool,
    pub background_unavailable_reason: Option<BackgroundUnavailableReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeFallbackLocale {
    Japanese,
    English,
    Bilingual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestPhase {
    AwaitingUiAck,
    AwaitingChoice,
    NativeFallback,
    Backgrounding,
    QuitRequested,
}

struct PendingCloseRequest {
    payload: CloseChoiceRequest,
    phase: RequestPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuitPhase {
    Idle,
    Requested,
    Running,
}

#[derive(Default)]
struct LifecycleState {
    pending_close: Option<PendingCloseRequest>,
    last_ui_locale: Option<NativeFallbackLocale>,
    native_fallback_quit_locale: Option<NativeFallbackLocale>,
    quit_phase: Option<QuitPhase>,
}

/// Serializes close-choice requests with app-wide exit requests.
#[derive(Default)]
pub struct WindowLifecycle {
    state: Mutex<LifecycleState>,
    next_request_id: AtomicU64,
}

impl WindowLifecycle {
    pub fn begin_close_request(
        &self,
        window_label: &str,
        tray_availability: TrayAvailability,
    ) -> Option<CloseChoiceRequest> {
        let mut state = self.state.lock();
        if state.pending_close.is_some()
            || state.quit_phase.unwrap_or(QuitPhase::Idle) != QuitPhase::Idle
        {
            return None;
        }
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed) + 1;
        let payload = CloseChoiceRequest {
            request_id,
            window_label: window_label.to_owned(),
            background_available: tray_availability == TrayAvailability::VerifiedRestorable,
            background_unavailable_reason: tray_availability.unavailable_reason(),
        };
        state.pending_close = Some(PendingCloseRequest {
            payload: payload.clone(),
            phase: RequestPhase::AwaitingUiAck,
        });
        Some(payload)
    }

    /// Records the UI's resolved locale and returns any close request emitted
    /// before its event listener was ready.
    pub fn register_ui(&self, locale: &str) -> Option<CloseChoiceRequest> {
        self.sync_ui_locale(locale);
        self.pending_close_request()
    }

    pub fn sync_ui_locale(&self, locale: &str) {
        let locale = if locale.starts_with("ja") {
            NativeFallbackLocale::Japanese
        } else if locale.starts_with("en") {
            NativeFallbackLocale::English
        } else {
            return;
        };
        self.state.lock().last_ui_locale = Some(locale);
    }

    pub fn last_ui_locale(&self) -> NativeFallbackLocale {
        self.state
            .lock()
            .last_ui_locale
            .unwrap_or(NativeFallbackLocale::Bilingual)
    }

    pub fn pending_close_request(&self) -> Option<CloseChoiceRequest> {
        self.state
            .lock()
            .pending_close
            .as_ref()
            .map(|pending| pending.payload.clone())
    }

    pub fn acknowledge_close_request(&self, request_id: u64) -> bool {
        let mut state = self.state.lock();
        let Some(pending) = state.pending_close.as_mut() else {
            return false;
        };
        if pending.payload.request_id != request_id {
            return false;
        }
        match pending.phase {
            RequestPhase::AwaitingUiAck => {
                pending.phase = RequestPhase::AwaitingChoice;
                true
            }
            // Registration may be repeated after a WebView reload; the active
            // request remains displayable until the user selects an action.
            RequestPhase::AwaitingChoice => true,
            RequestPhase::NativeFallback
            | RequestPhase::Backgrounding
            | RequestPhase::QuitRequested => false,
        }
    }

    /// Claim native fallback only while the web UI has not acknowledged the
    /// request. A late acknowledgement is then rejected by the same state lock.
    pub fn begin_native_fallback(&self, request_id: u64) -> Option<NativeFallbackLocale> {
        let mut state = self.state.lock();
        let pending = state.pending_close.as_mut()?;
        if pending.payload.request_id != request_id || pending.phase != RequestPhase::AwaitingUiAck
        {
            return None;
        }
        pending.phase = RequestPhase::NativeFallback;
        Some(
            state
                .last_ui_locale
                .unwrap_or(NativeFallbackLocale::Bilingual),
        )
    }

    pub fn resolve_native_fallback(&self, request_id: u64, quit: bool) -> bool {
        let mut state = self.state.lock();
        let Some(pending) = state.pending_close.as_ref() else {
            return false;
        };
        if pending.payload.request_id != request_id || pending.phase != RequestPhase::NativeFallback
        {
            return false;
        }
        if quit && state.quit_phase.unwrap_or(QuitPhase::Idle) != QuitPhase::Idle {
            return false;
        }
        state.pending_close = None;
        if quit {
            state.quit_phase = Some(QuitPhase::Requested);
            state.native_fallback_quit_locale = Some(
                state
                    .last_ui_locale
                    .unwrap_or(NativeFallbackLocale::Bilingual),
            );
        }
        true
    }

    pub fn select_close_action(
        &self,
        request_id: u64,
        choice: CloseChoice,
        tray_availability: TrayAvailability,
    ) -> CloseActionResult {
        let mut state = self.state.lock();
        let Some(pending) = state.pending_close.as_mut() else {
            return CloseActionResult::StaleRequest;
        };
        if pending.payload.request_id != request_id {
            return CloseActionResult::StaleRequest;
        }
        if pending.phase != RequestPhase::AwaitingChoice {
            return CloseActionResult::InvalidState;
        }
        match choice {
            CloseChoice::Cancel => {
                state.pending_close = None;
                CloseActionResult::Accepted
            }
            CloseChoice::Quit => {
                if state.quit_phase.unwrap_or(QuitPhase::Idle) != QuitPhase::Idle {
                    return CloseActionResult::InvalidState;
                }
                state.quit_phase = Some(QuitPhase::Requested);
                state.pending_close.as_mut().unwrap().phase = RequestPhase::QuitRequested;
                CloseActionResult::Accepted
            }
            CloseChoice::Background => {
                if !pending.payload.background_available
                    || tray_availability != TrayAvailability::VerifiedRestorable
                {
                    if tray_availability != TrayAvailability::VerifiedRestorable {
                        pending.payload.background_unavailable_reason =
                            tray_availability.unavailable_reason();
                    }
                    pending.payload.background_available = false;
                    return CloseActionResult::BackgroundUnavailable;
                }
                pending.phase = RequestPhase::Backgrounding;
                CloseActionResult::Accepted
            }
        }
    }

    pub fn complete_background_action(
        &self,
        request_id: u64,
        result: Result<(), BackgroundUnavailableReason>,
    ) -> bool {
        let mut state = self.state.lock();
        let Some(pending) = state.pending_close.as_mut() else {
            return false;
        };
        if pending.payload.request_id != request_id || pending.phase != RequestPhase::Backgrounding
        {
            return false;
        }
        match result {
            Ok(()) => state.pending_close = None,
            Err(reason) => {
                pending.phase = RequestPhase::AwaitingChoice;
                pending.payload.background_available = false;
                pending.payload.background_unavailable_reason = Some(reason);
            }
        }
        true
    }

    /// Admit an app.exit() call. The actual shutdown remains owned by the
    /// existing ExitRequested handler.
    pub fn request_quit(&self) -> bool {
        let mut state = self.state.lock();
        if state.quit_phase.unwrap_or(QuitPhase::Idle) != QuitPhase::Idle {
            return false;
        }
        state.quit_phase = Some(QuitPhase::Requested);
        true
    }

    pub fn request_native_fallback_quit(&self, locale: NativeFallbackLocale) -> bool {
        let mut state = self.state.lock();
        if state.quit_phase.unwrap_or(QuitPhase::Idle) != QuitPhase::Idle {
            return false;
        }
        state.quit_phase = Some(QuitPhase::Requested);
        state.native_fallback_quit_locale = Some(locale);
        true
    }

    pub fn take_native_fallback_retry_locale(&self) -> Option<NativeFallbackLocale> {
        self.state.lock().native_fallback_quit_locale.take()
    }

    /// Claim the one ExitRequested event that runs durable reconciliation and
    /// sidecar shutdown. Direct tray/OS exits enter from Idle.
    pub fn begin_exit_requested(&self) -> bool {
        let mut state = self.state.lock();
        if state.quit_phase == Some(QuitPhase::Running) {
            return false;
        }
        state.quit_phase = Some(QuitPhase::Running);
        if !state
            .pending_close
            .as_ref()
            .is_some_and(|pending| pending.phase == RequestPhase::QuitRequested)
        {
            state.pending_close = None;
        }
        true
    }

    /// Release the exit guard after shutdown failure and discard the failed
    /// close request so the frontend can start a fresh retry.
    pub fn finish_exit(&self, success: bool) -> bool {
        let mut state = self.state.lock();
        if state.quit_phase != Some(QuitPhase::Running) {
            return false;
        }
        if success {
            return true;
        }
        state.quit_phase = Some(QuitPhase::Idle);
        state.pending_close = None;
        true
    }

    pub fn is_quit_in_progress(&self) -> bool {
        self.state.lock().quit_phase.unwrap_or(QuitPhase::Idle) != QuitPhase::Idle
    }
}

pub fn tray_availability_for_icon<T>(
    icon: Option<&T>,
    host_availability: TrayAvailability,
) -> TrayAvailability {
    if icon.is_some() {
        host_availability
    } else {
        TrayAvailability::Unavailable
    }
}

#[doc(hidden)]
pub fn platform_tray_host_availability(
    tray_icon: Option<&tauri::tray::TrayIcon>,
) -> TrayAvailability {
    #[cfg(target_os = "macos")]
    {
        let _ = tray_icon;
        TrayAvailability::VerifiedRestorable
    }
    #[cfg(target_os = "windows")]
    {
        use tauri::Size;
        tray_icon
            .and_then(|icon| icon.rect().ok().flatten())
            .filter(|rect| match rect.size {
                Size::Physical(size) => size.width > 0 && size.height > 0,
                Size::Logical(size) => size.width > 0.0 && size.height > 0.0,
            })
            .map(|_| TrayAvailability::VerifiedRestorable)
            .unwrap_or(TrayAvailability::Unverified)
    }
    #[cfg(target_os = "linux")]
    {
        let _ = tray_icon;
        TrayAvailability::Unverified
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = tray_icon;
        TrayAvailability::Unverified
    }
}

pub trait WindowRestoreAdapter {
    fn show(&self) -> bool;
    fn unminimize(&self) -> bool;
    fn focus(&self) -> bool;
}

pub fn restore_window(window: &dyn WindowRestoreAdapter) -> bool {
    let shown = window.show();
    let unminimized = window.unminimize();
    let focused = window.focus();
    shown && unminimized && focused
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitRetryPlan {
    pub native_dialog: bool,
    pub locale: Option<NativeFallbackLocale>,
}

/// Choose a visible retry surface after shutdown failure, restoring a window
/// before relying on the web UI.
pub fn plan_exit_retry(
    window: Option<&dyn WindowRestoreAdapter>,
    native_fallback_locale: Option<NativeFallbackLocale>,
    last_ui_locale: NativeFallbackLocale,
) -> ExitRetryPlan {
    let restored = window.is_some_and(restore_window);
    let locale = native_fallback_locale.or_else(|| (!restored).then_some(last_ui_locale));
    ExitRetryPlan {
        native_dialog: locale.is_some(),
        locale,
    }
}
