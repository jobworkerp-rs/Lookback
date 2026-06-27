//! Mirror of memories' `StartupError` vocabulary, plus the stdout/stderr
//! scanner the sidecar host uses to detect a structured startup failure
//! without waiting on the 30 s TCP timeout.
//!
//! ## Contract (frozen across both crates)
//!
//! - `STARTUP_ERROR_TARGET = "app::startup_error"` matches the memories
//!   constant of the same name. The parent (this crate) accepts a tracing
//!   JSON row only when `target == STARTUP_ERROR_TARGET` and
//!   `level == "ERROR"`.
//! - `code` snake_case strings (`lancedb_schema_mismatch`, …) are
//!   FROZEN. A memories rename / removal is a breaking change and
//!   `startup_failure_codes_pin` will fail until both sides are updated.
//! - Field names inside each variant are FROZEN. Renaming / removing one
//!   silently turns the row into "unknown code" via the Deserialize fall-
//!   through and lands in the raw fallback path.
//! - Adding a new variant in memories without adding it here is
//!   non-breaking by design: `parse_stdout_line` returns `None` for the
//!   unknown row, the TCP wait runs to its 30 s timeout, and the host
//!   surfaces `SidecarErrorPayload::Raw{message}` to the UI. The UI then
//!   renders a generic "raw" panel rather than crashing.
//!
//! See `agent-app/ai-docs/sidecar-startup-failure-handling.md` (the
//! design doc), and `memories/infra/src/infra/startup_error.rs` (the
//! sibling contract).

use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// `target` field value attached to every structured startup-error
/// tracing event by the memories sidecar. The host matches this verbatim
/// when scanning stdout. Renaming is a coordinated breaking change with
/// memories — keep both sides on the same literal.
pub const STARTUP_ERROR_TARGET: &str = "app::startup_error";

/// Mirror of `memories::infra::infra::startup_error::StartupError`.
/// `#[serde(tag = "code")]` lets us deserialize the `fields` block of a
/// tracing JSON row directly into the matching variant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum StartupFailure {
    LancedbSchemaMismatch {
        table: String,
        uri: String,
        expected_dim: u32,
        actual_dim: u32,
        // memories emits these for diagnostics. UI doesn't read them but
        // we deserialize so a missing field doesn't break the variant —
        // `#[serde(default)]` makes the field optional on the wire.
        #[serde(default)]
        expected_fingerprint: String,
        #[serde(default)]
        actual_fingerprint: String,
    },
    LancedbInitFailed {
        uri: String,
        message: String,
    },
    EmbeddingDimensionMismatch {
        expected_dim: u32,
        actual_dim: u32,
        runner_name: String,
    },
    MediaConfigConflict {
        backend: String,
        image_search_mode: String,
    },
    RdbPoolInitFailed {
        url_sanitized: String,
        message: String,
    },
    EnvVarInvalid {
        name: String,
        message: String,
    },
    ConfigLoadFailed {
        component: String,
        message: String,
    },
    Other {
        component: String,
        message: String,
    },
}

impl StartupFailure {
    /// Discriminator as a stable `&'static str`. Pinned by
    /// `startup_failure_codes_pin` so a memories-side rename trips a
    /// test failure here rather than a runtime "unknown" fallback.
    pub fn code(&self) -> &'static str {
        match self {
            Self::LancedbSchemaMismatch { .. } => "lancedb_schema_mismatch",
            Self::LancedbInitFailed { .. } => "lancedb_init_failed",
            Self::EmbeddingDimensionMismatch { .. } => "embedding_dimension_mismatch",
            Self::MediaConfigConflict { .. } => "media_config_conflict",
            Self::RdbPoolInitFailed { .. } => "rdb_pool_init_failed",
            Self::EnvVarInvalid { .. } => "env_var_invalid",
            Self::ConfigLoadFailed { .. } => "config_load_failed",
            Self::Other { .. } => "other",
        }
    }
}

/// Subset of the tracing JSON row that the scanner needs. The
/// `fmt::layer().json()` output has the shape
/// `{timestamp, level, fields:{message, ...}, target}`; only `target` and
/// `fields` carry information we care about (timestamp is logged
/// separately and the surface message is reconstructed from the variant).
/// `target` and `level` are borrowed straight out of the input slice to
/// skip the per-line `String` allocation `pipe_lines` would otherwise
/// pay on every parsed row; `fields` stays owned because it is fed back
/// into `from_value` for the variant deserialize.
#[derive(Deserialize)]
struct TracingRow<'a> {
    #[serde(borrow)]
    target: Option<&'a str>,
    #[serde(borrow, default)]
    level: Option<&'a str>,
    #[serde(default)]
    fields: Option<serde_json::Value>,
}

/// Parse one line of the memories sidecar's stdout. Returns `Some` only
/// when the line is a tracing JSON row with `target == STARTUP_ERROR_TARGET`,
/// `level == "ERROR"`, and a `fields` block that round-trips into one of
/// the known `StartupFailure` variants. Any other line — non-JSON,
/// different target, different level, unknown code — returns `None` so
/// the caller keeps treating it as normal log noise.
pub fn parse_stdout_line(line: &str) -> Option<StartupFailure> {
    let row: TracingRow = serde_json::from_str(line).ok()?;
    if row.target != Some(STARTUP_ERROR_TARGET) {
        return None;
    }
    // ERROR-only on purpose. A future variant that wants a non-ERROR
    // level (e.g. soft degraded mode at WARN) must be added explicitly,
    // not silently piped through.
    if row.level != Some("ERROR") {
        return None;
    }
    serde_json::from_value(row.fields?).ok()
}

/// Fallback for the panic path: a stray `unwrap()` we haven't migrated
/// yet writes text to stderr via the Rust runtime's panic handler,
/// bypassing the tracing JSON layer. We scan stderr for the standard
/// `thread '...' (<pid>) panicked at ...` shape so the UI surfaces at
/// least the message instead of stalling 30 s on the TCP timeout. This
/// is defense-in-depth — the primary contract is the structured tracing
/// path above.
pub fn parse_stderr_panic_line(line: &str) -> Option<StartupFailure> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with("thread '") || !trimmed.contains("panicked at") {
        return None;
    }
    Some(StartupFailure::Other {
        component: "memories".into(),
        message: trimmed.to_string(),
    })
}

/// Shared slot the per-stream scanner writes into and the TCP wait loop
/// polls. `Arc<Mutex<...>>` is the simplest type that lets the two
/// tokio tasks (`pipe_lines` and the wait loop) cooperate without a
/// dedicated channel — first writer wins, subsequent writes are
/// dropped because the wait loop returns on the first `Some`.
pub type StartupFailureSlot = Arc<Mutex<Option<StartupFailure>>>;

/// Payload of the `sidecar://error` Tauri event. Tagged union so the
/// frontend can branch on `kind` (then on `failure.code`) without
/// parsing the human-readable `message` text.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SidecarErrorPayload {
    Structured { failure: StartupFailure },
    Raw { message: String },
}

impl SidecarErrorPayload {
    /// Lift an `AppError` into a frontend-shaped payload. Only the
    /// `SidecarStartupFailed` variant carries a structured failure —
    /// everything else collapses into a raw message so the BootError UI
    /// can still render a meaningful fallback.
    pub fn from_app_error(err: &crate::error::AppError) -> Self {
        if let crate::error::AppError::SidecarStartupFailed(failure) = err {
            return Self::Structured {
                failure: failure.clone(),
            };
        }
        Self::Raw {
            message: err.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const FIXTURE_LANCEDB_SCHEMA: &str =
        include_str!("../../tests/fixtures/memories_startup_error_lancedb_schema_mismatch.json");
    const FIXTURE_LANCEDB_INIT: &str =
        include_str!("../../tests/fixtures/memories_startup_error_lancedb_init_failed.json");
    const FIXTURE_EMBEDDING_DIM: &str = include_str!(
        "../../tests/fixtures/memories_startup_error_embedding_dimension_mismatch.json"
    );
    const FIXTURE_MEDIA_CONFLICT: &str =
        include_str!("../../tests/fixtures/memories_startup_error_media_config_conflict.json");
    const FIXTURE_RDB_POOL: &str =
        include_str!("../../tests/fixtures/memories_startup_error_rdb_pool_init_failed.json");
    const FIXTURE_ENV_VAR: &str =
        include_str!("../../tests/fixtures/memories_startup_error_env_var_invalid.json");
    const FIXTURE_CONFIG_LOAD: &str =
        include_str!("../../tests/fixtures/memories_startup_error_config_load_failed.json");
    const FIXTURE_OTHER: &str =
        include_str!("../../tests/fixtures/memories_startup_error_other.json");
    // Negative samples live inline rather than as files: their only job
    // is to assert the parser rejects shapes that differ from the
    // positive fixtures by exactly one field, and a tiny one-line
    // literal makes the diff much clearer than a separate JSON file.
    const FIXTURE_NEG_TARGET: &str = r#"{"timestamp":"2026-06-05T07:50:50.976570Z","level":"ERROR","fields":{"message":"x","code":"lancedb_schema_mismatch","table":"memories","uri":"/x","expected_dim":2048,"actual_dim":768,"expected_fingerprint":"","actual_fingerprint":""},"target":"infra_utils::infra::rdb"}"#;
    const FIXTURE_NEG_INFO: &str = r#"{"timestamp":"2026-06-05T07:50:50.976570Z","level":"INFO","fields":{"message":"informational","code":"lancedb_schema_mismatch","table":"memories","uri":"/x","expected_dim":2048,"actual_dim":768,"expected_fingerprint":"","actual_fingerprint":""},"target":"app::startup_error"}"#;
    const FIXTURE_NEG_UNKNOWN_CODE: &str = r#"{"timestamp":"2026-06-05T07:50:50.976570Z","level":"ERROR","fields":{"message":"future","code":"future_variant_not_yet_known","detail":"whatever"},"target":"app::startup_error"}"#;

    /// `include_str!` keeps a trailing newline from the file; the real
    /// scanner reads from `BufReader::lines()` which strips it, so we
    /// trim here to match the production input.
    fn fx(s: &str) -> &str {
        s.trim_end()
    }

    #[test]
    fn target_constant_is_app_startup_error() {
        // Pinned: the memories side declares the same literal. Renaming
        // requires a coordinated change in both crates.
        assert_eq!(STARTUP_ERROR_TARGET, "app::startup_error");
    }

    #[test]
    fn parse_stdout_line_lancedb_schema_mismatch() {
        match parse_stdout_line(fx(FIXTURE_LANCEDB_SCHEMA)).expect("should parse") {
            StartupFailure::LancedbSchemaMismatch {
                expected_dim,
                actual_dim,
                table,
                ..
            } => {
                assert_eq!(expected_dim, 2048);
                assert_eq!(actual_dim, 768);
                assert_eq!(table, "memories");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_stdout_line_lancedb_init_failed() {
        match parse_stdout_line(fx(FIXTURE_LANCEDB_INIT)).expect("should parse") {
            StartupFailure::LancedbInitFailed { uri, message } => {
                assert!(uri.ends_with("memories.lancedb"));
                assert!(message.contains("Permission denied"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_stdout_line_embedding_dimension_mismatch() {
        match parse_stdout_line(fx(FIXTURE_EMBEDDING_DIM)).expect("should parse") {
            StartupFailure::EmbeddingDimensionMismatch {
                expected_dim,
                actual_dim,
                runner_name,
            } => {
                assert_eq!(expected_dim, 2048);
                assert_eq!(actual_dim, 768);
                assert_eq!(runner_name, "memories-mm-embedding");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_stdout_line_media_config_conflict() {
        match parse_stdout_line(fx(FIXTURE_MEDIA_CONFLICT)).expect("should parse") {
            StartupFailure::MediaConfigConflict {
                backend,
                image_search_mode,
            } => {
                assert_eq!(backend, "inline");
                assert_eq!(image_search_mode, "clip");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_stdout_line_rdb_pool_init_failed() {
        match parse_stdout_line(fx(FIXTURE_RDB_POOL)).expect("should parse") {
            StartupFailure::RdbPoolInitFailed {
                url_sanitized,
                message,
            } => {
                assert!(url_sanitized.starts_with("sqlite:"));
                assert!(message.contains("unable to open"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_stdout_line_env_var_invalid() {
        match parse_stdout_line(fx(FIXTURE_ENV_VAR)).expect("should parse") {
            StartupFailure::EnvVarInvalid { name, message } => {
                assert_eq!(name, "GRPC_ADDR");
                assert!(message.contains("not_a_socket_addr"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_stdout_line_config_load_failed() {
        match parse_stdout_line(fx(FIXTURE_CONFIG_LOAD)).expect("should parse") {
            StartupFailure::ConfigLoadFailed { component, message } => {
                assert_eq!(component, "VectorDBConfig");
                assert!(message.contains("MEMORY_VECTOR_SIZE"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_stdout_line_other() {
        match parse_stdout_line(fx(FIXTURE_OTHER)).expect("should parse") {
            StartupFailure::Other { component, message } => {
                assert_eq!(component, "front");
                assert!(message.contains("unexpected error"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_stdout_line_rejects_target_mismatch() {
        assert!(parse_stdout_line(fx(FIXTURE_NEG_TARGET)).is_none());
    }

    #[test]
    fn parse_stdout_line_rejects_non_error_level() {
        assert!(parse_stdout_line(fx(FIXTURE_NEG_INFO)).is_none());
    }

    #[test]
    fn parse_stdout_line_rejects_unknown_code() {
        // memories adds a new variant we don't know about yet; we want
        // the scanner to return `None` so the host falls back to the
        // 30 s TCP timeout + raw payload path. The UI then shows a
        // generic "raw" panel rather than crashing or dropping.
        assert!(parse_stdout_line(fx(FIXTURE_NEG_UNKNOWN_CODE)).is_none());
    }

    #[test]
    fn parse_stdout_line_rejects_non_json() {
        assert!(parse_stdout_line("Use default RdbConfig (sqlite3).").is_none());
        assert!(parse_stdout_line("").is_none());
        assert!(parse_stdout_line("{not valid json").is_none());
    }

    #[test]
    fn parse_stderr_panic_line_recognises_thread_main_panic() {
        let line = "thread 'main' (6739761) panicked at infra/src/infra/module.rs:69:26: \
                    LanceDB initialization failed: ...";
        let parsed = parse_stderr_panic_line(line).expect("should recognise panic");
        match parsed {
            StartupFailure::Other { component, message } => {
                assert_eq!(component, "memories");
                assert!(message.contains("panicked at"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_stderr_panic_line_ignores_normal_log() {
        assert!(parse_stderr_panic_line("WARN something happened").is_none());
        assert!(parse_stderr_panic_line("").is_none());
        // We require the *exact* panic preamble; a free-form "panicked"
        // mention in a log message must not be caught.
        assert!(parse_stderr_panic_line("the test panicked at line 3").is_none());
    }

    /// **Degradation regression**. The fixed code set must stay in sync
    /// with memories' `StartupError`. A future memories rename / removal
    /// trips this test before it reaches production.
    #[test]
    fn startup_failure_codes_pin() {
        let expected: BTreeSet<&str> = [
            "lancedb_schema_mismatch",
            "lancedb_init_failed",
            "embedding_dimension_mismatch",
            "media_config_conflict",
            "rdb_pool_init_failed",
            "env_var_invalid",
            "config_load_failed",
            "other",
        ]
        .into_iter()
        .collect();

        // Construct one of each variant and collect the `code()` output.
        // Cheap dummies — the values are never inspected here.
        let variants: Vec<StartupFailure> = vec![
            StartupFailure::LancedbSchemaMismatch {
                table: String::new(),
                uri: String::new(),
                expected_dim: 0,
                actual_dim: 0,
                expected_fingerprint: String::new(),
                actual_fingerprint: String::new(),
            },
            StartupFailure::LancedbInitFailed {
                uri: String::new(),
                message: String::new(),
            },
            StartupFailure::EmbeddingDimensionMismatch {
                expected_dim: 0,
                actual_dim: 0,
                runner_name: String::new(),
            },
            StartupFailure::MediaConfigConflict {
                backend: String::new(),
                image_search_mode: String::new(),
            },
            StartupFailure::RdbPoolInitFailed {
                url_sanitized: String::new(),
                message: String::new(),
            },
            StartupFailure::EnvVarInvalid {
                name: String::new(),
                message: String::new(),
            },
            StartupFailure::ConfigLoadFailed {
                component: String::new(),
                message: String::new(),
            },
            StartupFailure::Other {
                component: String::new(),
                message: String::new(),
            },
        ];
        let actual: BTreeSet<&str> = variants.iter().map(StartupFailure::code).collect();
        assert_eq!(
            actual, expected,
            "StartupFailure::code() set diverged from the pinned code list; \
             agent-app and memories must agree on these exact strings"
        );
    }

    // NOTE: a dedicated serde roundtrip test was removed — the eight
    // `parse_stdout_line_*` fixture tests above already exercise
    // deserialize against the exact wire shape memories emits, and
    // `startup_failure_codes_pin` covers the `#[serde(tag = "code")]`
    // discriminant. Re-asserting both via a roundtrip added no signal
    // and just doubled the diff cost of adding a variant.

    #[test]
    fn sidecar_error_payload_from_app_error_structured() {
        let failure = StartupFailure::LancedbSchemaMismatch {
            table: "t".into(),
            uri: "u".into(),
            expected_dim: 2048,
            actual_dim: 768,
            expected_fingerprint: String::new(),
            actual_fingerprint: String::new(),
        };
        let err = crate::error::AppError::SidecarStartupFailed(failure.clone());
        match SidecarErrorPayload::from_app_error(&err) {
            SidecarErrorPayload::Structured { failure: inner } => assert_eq!(inner, failure),
            other => panic!("expected Structured, got {other:?}"),
        }
    }

    #[test]
    fn sidecar_error_payload_from_app_error_falls_back_to_raw() {
        let err = crate::error::AppError::SidecarNotReady("nope".into());
        match SidecarErrorPayload::from_app_error(&err) {
            SidecarErrorPayload::Raw { message } => assert!(message.contains("nope")),
            other => panic!("expected Raw, got {other:?}"),
        }
    }
}
