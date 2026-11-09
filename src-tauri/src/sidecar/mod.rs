pub(crate) mod embedding_migration;
pub mod generation_workers;
pub mod lifecycle;
pub mod memories_migration;
pub mod ports;
pub(crate) mod query_embedding_worker;
pub mod readiness;
pub mod reaper;
pub mod startup_error;

pub use lifecycle::{
    ManagedChildHomeSource, ManagedJobworkerpAdmissionGuard, ManagedJobworkerpRuntimeObservation,
    ManagedJobworkerpSdkSourceCheck, ManagedJobworkerpSdkSourceFileStamp,
    ManagedJobworkerpSdkSourceSnapshot, ManagedJobworkerpSdkSourceStamp,
    ManagedJobworkerpSdkSourceUnavailableReason, ManagedJobworkerpSpawnSnapshot,
    ManagedSdkConfigSourceStamp, McpRuntimeStatus, RestartPlan, SidecarConfig, SidecarEndpoints,
    SidecarKind, SidecarStartReport, SidecarWarning, SidecarWarningKind, Sidecars,
};
// `STARTUP_ERROR_TARGET` and `StartupFailureSlot` stay module-private:
// the target literal is half of a frozen contract with memories (not a
// sibling API) and the slot type is only constructed inside `lifecycle`.
pub use startup_error::{SidecarErrorPayload, StartupFailure};
