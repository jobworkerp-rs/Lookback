//! Opt-in end-to-end coverage for progress chunks from the real summary sidecars.
//!
//! This uses a fresh, empty data root. Each requested period still exercises the
//! batch loop (`1/1`), while the empty source store keeps the single-period
//! workers on their no-input path and avoids calling an LLM provider.
//!
//! ```sh
//! LOOKBACK_JOBWORKERP_BIN="$PWD/src-tauri/bin/all-in-one-x86_64-unknown-linux-gnu" \
//! LOOKBACK_MEMORIES_BIN="$PWD/src-tauri/bin/front-x86_64-unknown-linux-gnu" \
//! LOOKBACK_CONDUCTOR_BIN="$PWD/src-tauri/bin/conductor-main-x86_64-unknown-linux-gnu" \
//! LOOKBACK_PLUGINS_SRC="$PWD/src-tauri/plugins" \
//!   cargo test -p lookback-tauri --test generation_progress_e2e -- --ignored --nocapture
//! ```

use std::{
    collections::BTreeSet,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    time::Duration,
};

use futures::FutureExt;
use lookback_tauri_lib::{
    data::{DataPaths, paths},
    jobworkerp::{JobworkerpHandle, ProgressEvent},
    sidecar::{SidecarConfig, Sidecars},
};
use serde_json::Value;

const DISPATCH_TIMEOUT: Duration = Duration::from_secs(60);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const WORKFLOW_TIMEOUT_SECONDS: u64 = 10 * 60;

fn required_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| {
        panic!("live generation e2e requires {name} to point at a staged binary or directory")
    }))
}

fn progress_pair(value: &Value) -> Option<(u64, u64)> {
    let object = value.as_object()?;
    let processed = object
        .get("progress_processed")
        .or_else(|| object.get("progressProcessed"))
        .and_then(value_as_u64)?;
    let total = object
        .get("progress_total")
        .or_else(|| object.get("progressTotal"))
        .and_then(value_as_u64)?;
    Some((processed, total))
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse::<u64>().ok())
}

#[derive(Debug, Default)]
struct PayloadSummary {
    parsed_payloads: usize,
    unparsed_payloads: usize,
    unparsed_mentions_progress: bool,
    field_names: BTreeSet<String>,
    progress_pairs: Vec<(u64, u64)>,
    statuses: BTreeSet<String>,
    error_messages: BTreeSet<String>,
    output_shapes: BTreeSet<String>,
}

fn value_shape(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string-redacted",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn safe_status(value: &Value) -> String {
    let Some(status) = value.as_str() else {
        return "<non-string-or-unset>".into();
    };
    match status.to_ascii_lowercase().as_str() {
        "running" => "running",
        "completed" => "completed",
        "success" => "success",
        "faulted" => "faulted",
        "failed" => "failed",
        "warning" => "warning",
        "done" => "done",
        "cancelled" | "canceled" => "cancelled",
        _ => "<other>",
    }
    .into()
}

fn inspect_payload(value: &Value, summary: &mut PayloadSummary, depth: usize) {
    if depth > 32 {
        return;
    }
    if let Some(pair) = progress_pair(value) {
        summary.progress_pairs.push(pair);
    }
    match value {
        Value::Array(items) => {
            for item in items {
                inspect_payload(item, summary, depth + 1);
            }
        }
        Value::Object(fields) => {
            for (key, item) in fields {
                summary.field_names.insert(key.clone());
                match key.to_ascii_lowercase().as_str() {
                    "status" => {
                        summary.statuses.insert(safe_status(item));
                    }
                    "errormessage" | "error_message" => {
                        let shape = match item {
                            Value::Null => "null",
                            Value::String(value) if value.is_empty() => "empty-string",
                            Value::String(_) => "nonempty-string-redacted",
                            other => value_shape(other),
                        };
                        summary.error_messages.insert(shape.into());
                    }
                    "output" => {
                        summary.output_shapes.insert(value_shape(item).into());
                    }
                    _ => {}
                }
                inspect_payload(item, summary, depth + 1);
            }
        }
        Value::String(encoded) => {
            if let Ok(nested) = serde_json::from_str::<Value>(encoded) {
                inspect_payload(&nested, summary, depth + 1);
            }
        }
        _ => {}
    }
}

fn summarize_payloads(payloads: &[String]) -> PayloadSummary {
    let mut summary = PayloadSummary::default();
    for payload in payloads {
        match serde_json::from_str::<Value>(payload) {
            Ok(value) => {
                summary.parsed_payloads += 1;
                inspect_payload(&value, &mut summary, 0);
            }
            Err(_) => {
                summary.unparsed_payloads += 1;
                summary.unparsed_mentions_progress |=
                    payload.to_ascii_lowercase().contains("progress");
            }
        }
    }
    summary
}

async fn assert_stream_reports_period_progress(
    handle: &JobworkerpHandle,
    worker_name: &str,
    input: Value,
    memories_port: u16,
) {
    let stream = tokio::time::timeout(
        DISPATCH_TIMEOUT,
        handle.dispatch_stream_with_timeout(
            worker_name,
            input,
            WORKFLOW_TIMEOUT_SECONDS,
            Some("run"),
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("dispatching {worker_name} timed out"))
    .unwrap_or_else(|error| panic!("dispatching {worker_name} failed: {error}"));

    let mut chunks = Vec::new();
    let mut terminal_texts = Vec::new();
    let mut got_end = false;
    tokio::time::timeout(
        DRAIN_TIMEOUT,
        stream.drain(|event| match event {
            ProgressEvent::Chunk { text } => chunks.push(text),
            ProgressEvent::End { final_text } => {
                got_end = true;
                terminal_texts.extend(final_text);
            }
        }),
    )
    .await
    .unwrap_or_else(|_| panic!("draining {worker_name} timed out"))
    .unwrap_or_else(|error| panic!("draining {worker_name} failed: {error}"));
    assert!(
        got_end,
        "{worker_name} must terminate with a stream End event"
    );

    let chunk_summary = summarize_payloads(&chunks);
    let terminal_summary = summarize_payloads(&terminal_texts);
    assert!(
        chunk_summary
            .progress_pairs
            .iter()
            .any(|pair| pair.0 > 0 && pair.1 > 0),
        "[DEBUG-GENPROG] {worker_name} ended without a nonzero data-chunk progress pair; chunks={chunk_summary:?}; terminal={terminal_summary:?}"
    );
    assert!(
        chunk_summary.progress_pairs.contains(&(1, 1)),
        "{worker_name} must report its single requested period as 1/1 in a data chunk; observed {:?}",
        chunk_summary.progress_pairs
    );
    eprintln!(
        "observed {worker_name} progress {:?} on memories port {memories_port}",
        chunk_summary.progress_pairs
    );
}

fn empty_scratch_env(root: &Path) -> PathBuf {
    let env_file = root.join(".env");
    std::fs::write(
        &env_file,
        "MEMORY_CACHE_NUM_COUNTERS=12960\n\
         MEMORY_CACHE_MAX_COST=12960\n\
         MEMORY_CACHE_USE_METRICS=true\n\
         WORKER_DEFAULT_CONCURRENCY=4\n\
         MCP_ENABLED=false\n\
         MEMORY_VECTOR_ENABLED=false\n\
         THREAD_VECTOR_ENABLED=false\n\
         REFLECTION_INTENT_VECTOR_ENABLED=false\n",
    )
    .expect("write isolated sidecar environment");
    env_file
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "starts real all-in-one/front/conductor sidecars; see this test's invocation above"]
async fn daily_weekly_and_monthly_batches_stream_nonzero_period_progress_without_llm_calls() {
    let scratch = tempfile::Builder::new()
        .prefix("generation-progress-live-")
        .tempdir_in("/tmp/opencode")
        .expect("create isolated scratch root");
    let data = DataPaths::with_root(scratch.path());
    data.ensure().expect("create scratch data root");
    let env_file = empty_scratch_env(scratch.path());
    lookback_tauri_lib::commands::issue_tracker_settings::save_issue_tracker_settings(
        &data.issue_tracker_settings_path(),
        &lookback_tauri_lib::commands::issue_tracker_settings::IssueTrackerSettings {
            enabled: Some(false),
        },
    )
    .expect("disable Issue Tracker in the isolated data root");

    let plugins_src = required_path("LOOKBACK_PLUGINS_SRC");
    lookback_tauri_lib::plugins::stage_plugins_from(&plugins_src, &data.plugins_dir())
        .expect("stage existing plugin binaries into the scratch root");

    let config = SidecarConfig {
        jobworkerp_bin: required_path("LOOKBACK_JOBWORKERP_BIN"),
        memories_bin: required_path("LOOKBACK_MEMORIES_BIN"),
        // Startup migration is disabled for this throwaway, empty database.
        memories_db_migrate_bin: PathBuf::from("/bin/true"),
        startup_database_migration_enabled: false,
        conductor_bin: required_path("LOOKBACK_CONDUCTOR_BIN"),
        data: data.clone(),
        worker_yaml_paths: vec![paths::llm_workers_yaml().expect("resolve worker YAML")],
        issue_tracker_server_bin: None,
        function_set_yaml_paths: vec![
            paths::function_sets_yaml().expect("resolve FunctionSets YAML"),
        ],
        reflection_dispatch_enabled: false,
        auto_embedding_enabled: false,
        workflows_dir: paths::workflows_bundle_dir().ok(),
        lance_language_model_home: data.lance_language_model_home(),
        lindera_dict_staged: false,
        llm_model: None,
        llm_hf_repo: None,
        llm_ctx_size: None,
        llm_kv_cache_type: None,
        env_file: Some(env_file),
    };
    let sidecars = Sidecars::new(config);
    let result = AssertUnwindSafe(async {
        let report = sidecars.start().await.expect("start isolated sidecars");
        let memories_port = report
            .endpoints
            .memories_port
            .expect("local memories front endpoint");
        eprintln!(
            "started isolated all-in-one/front/conductor sidecars: jobworkerp={:?}, memories={}, conductor={:?}; scratch={}",
            report.endpoints.jobworkerp_port,
            memories_port,
            report.endpoints.conductor_port,
            scratch.path().display(),
        );
        if !report.warnings.is_empty() {
            eprintln!("sidecar startup warnings: {}", report.warnings.len());
        }

        let handle = JobworkerpHandle::connect(
            &report.endpoints.jobworkerp_url().expect("local jobworkerp"),
        )
            .await
            .expect("connect to scratch jobworkerp");
        let memories = serde_json::json!({
            "user_id": 765_432_109,
            "memories_grpc_host": "127.0.0.1",
            "memories_grpc_port": memories_port,
            "output_language": "en",
        });

        for (worker_name, period_input) in [
            (
                "memories-daily-summary-batch",
                serde_json::json!({"target_dates": ["2024-01-02"]}),
            ),
            (
                "memories-weekly-summary-batch",
                serde_json::json!({"target_weeks": ["2024-W02"]}),
            ),
            (
                "memories-monthly-summary-batch",
                serde_json::json!({"target_months": ["2024-01"]}),
            ),
        ] {
            let mut input = memories.as_object().expect("base input object").clone();
            input.extend(
                period_input
                    .as_object()
                    .expect("period input object")
                    .clone(),
            );
            assert_stream_reports_period_progress(
                &handle,
                worker_name,
                Value::Object(input),
                memories_port,
            )
            .await;
        }
    })
    .catch_unwind()
    .await;

    // Await orderly shutdown before TempDir removes the SQLite files, including
    // when an assertion or startup expectation above unwinds.
    let stop_result = sidecars.stop().await;
    if let Err(payload) = result {
        if let Err(error) = stop_result {
            eprintln!("sidecar cleanup also failed after test panic: {error}");
        }
        panic::resume_unwind(payload);
    }
    stop_result.expect("stop isolated sidecars");
}
