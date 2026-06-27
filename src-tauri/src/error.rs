use serde::Serialize;

use crate::sidecar::startup_error::StartupFailure;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("sidecar not ready: {0}")]
    SidecarNotReady(String),

    /// The memories sidecar emitted a structured startup failure on its
    /// tracing stdout (or, fallback, a panic line on stderr) — we caught
    /// it before the 30 s TCP wait elapsed. The `code` is what the UI
    /// branches on; the inner payload carries the recovery hints
    /// (`expected_dim` / `actual_dim` etc.).
    #[error("sidecar startup failed: {}", .0.code())]
    SidecarStartupFailed(StartupFailure),

    /// Another Lookback instance is already running against the same data
    /// directory (it holds the `sidecar.lock`). We refuse to spawn a second set
    /// of sidecars — they would fight over the SQLite/LanceDB files and ports,
    /// and overwriting the shared `sidecar.pids` would later let a third launch
    /// mistake this instance's live children for crash orphans. The app window
    /// still opens so this message can be surfaced to the user.
    #[error(
        "another Lookback instance is already running with the same data directory; \
         close it before launching again, or point this instance at a different data root"
    )]
    AnotherInstanceRunning,

    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),

    #[error("transport error: {0}")]
    Transport(#[from] tonic::transport::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("config: {0}")]
    Config(String),

    #[error("worker registration: {0}")]
    WorkerRegistration(String),

    /// Failure interacting with the jobworkerp gRPC API (connect, find
    /// worker, enqueue stream). `WorkerRegistration` stays reserved for
    /// the YAML-apply path so the UI can keep distinguishing
    /// "workers couldn't be installed" from "an individual dispatch failed".
    #[error("jobworkerp: {0}")]
    Jobworkerp(String),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
