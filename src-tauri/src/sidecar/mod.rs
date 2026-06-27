pub mod generation_workers;
pub mod lifecycle;
pub mod ports;
pub mod reaper;
pub mod startup_error;

pub use lifecycle::{
    SidecarConfig, SidecarEndpoints, SidecarStartReport, SidecarWarning, SidecarWarningKind,
    Sidecars,
};
// `STARTUP_ERROR_TARGET` and `StartupFailureSlot` stay module-private:
// the target literal is half of a frozen contract with memories (not a
// sibling API) and the slot type is only constructed inside `lifecycle`.
pub use startup_error::{SidecarErrorPayload, StartupFailure};
