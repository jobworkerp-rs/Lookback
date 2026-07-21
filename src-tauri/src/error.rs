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

    #[error(
        "memory_kind migration is required for {db_path}; run the bundled migrate-memory-kind client-apply procedure before starting Lookback"
    )]
    MemoryKindMigrationRequired { db_path: String },

    #[error("memory_kind migration refused unexpected data at {db_path}: {reason}")]
    UnexpectedMemoryData { db_path: String, reason: String },

    #[error("memory_kind database schema is invalid at {db_path}: {reason}")]
    MemoryKindDatabaseSchemaInvalid { db_path: String, reason: String },

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

    /// The local vector store (LanceDB) could not be opened at the
    /// configured embedding dimension, so the sidecar was restarted in a
    /// degraded mode with vector features disabled. Embedding-dependent
    /// commands (semantic / hybrid / intent search, import, generation)
    /// refuse to run in this state and surface this error; the fix is to
    /// switch the embedding model back to the matching dimension in
    /// Settings. Only raised in local connection mode — remote mode routes
    /// embedding to the remote sidecar, which is unaffected.
    #[error(
        "ローカルのベクトルストアが次元不一致で無効化されています（期待 {expected_dim} 次元 / 実際 {actual_dim} 次元）。\
         設定で embedding モデルを一致する次元に変更してください"
    )]
    VectorStoreDegraded { expected_dim: u32, actual_dim: u32 },

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
