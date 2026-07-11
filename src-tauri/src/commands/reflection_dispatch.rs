//! Tauri command for the "Generate reflection" button.
//!
//! Streams the `memories-reflection-batch` named worker via
//! `jobworkerp::run_cancellable_named_stream`. The command returns
//! immediately with a `job_id_hint` so the UI can open a progress slot;
//! the actual stream is consumed in a detached task that emits
//! `reflection://step { job_id, status, message }` events.
//!
//! Cancellation uses the shared `dispatch_in_flight` map: the frontend
//! calls `reflection_cancel(dispatch_id)` → flips the cancel token +
//! issues `JobService/Delete` on the live job.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::error::{AppError, AppResult};
use crate::jobworkerp::{StreamEvent, run_cancellable_named_stream};

use super::connection::MemoriesCallback;
use super::import::{
    REFLECTION_PROMPT_VERSION, summarize_workflow_chunk, summarize_workflow_error,
};
use super::{
    AppState, GeneratedRefreshScope, StepStatus, cancel_dispatch_inner, emit_event,
    emit_generated_refresh, thread_reflection_single_completed,
};

// `pub(crate)` so the Settings queue card (`background_jobs.rs`) can
// classify counts by this name instead of carrying its own copy.
pub(crate) const REFLECTION_WORKER_NAME: &str = "memories-reflection-batch";
const REFLECTION_EVENT: &str = "reflection://step";

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EnqueueReflectionJobRequest {
    pub user_id: Option<i64>,
    /// Optional epoch-ms lower bound. Unset = all threads for the user
    /// (subject to single-workflow eligibility checks).
    pub updated_after_ms: Option<i64>,
    pub prompt_version: Option<String>,
    /// Cancel-key. The frontend generates a UUID and the Stop button
    /// forwards it to `reflection_cancel`.
    #[serde(default)]
    pub dispatch_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnqueueReflectionJobResponse {
    /// Frontend correlates `reflection://step` events with the toast
    /// it just opened. Synthesized on dispatch so we can return before
    /// the gRPC enqueue completes.
    pub job_id_hint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReflectionStepUpdate {
    pub job_id: String,
    pub status: StepStatus,
    pub message: Option<String>,
}

#[tauri::command]
pub async fn enqueue_reflection_job(
    app: AppHandle,
    state: State<'_, AppState>,
    req: EnqueueReflectionJobRequest,
) -> AppResult<EnqueueReflectionJobResponse> {
    // Reflection generation writes intent embeddings into the local vector
    // store; refuse when it is degraded (local mode only).
    state.ensure_local_embedding_available()?;
    let callback = state.resolve_targets()?.memories_callback()?;
    // Presence guard: the `memories-reflection-batch` worker is registered from
    // this bundled YAML (llm-workers.yaml `$file:`), so a missing bundle means
    // the dispatch below would fail with `WorkerNotFound`. Surface the clearer
    // packaging error up front. The path itself is no longer relayed — the batch
    // resolves its language-specific single worker by name.
    resolve_reflection_batch_yaml().ok_or_else(|| {
        AppError::Config(
            "thread-reflection-batch.yaml not bundled — package workflows into Tauri resources to enable production builds".into(),
        )
    })?;
    let llm_worker_name = state.active_llm_worker_name();
    let input = build_workflow_input(
        req.user_id.unwrap_or(1),
        &callback,
        &state.active_output_language(),
        req.prompt_version
            .as_deref()
            .unwrap_or(REFLECTION_PROMPT_VERSION),
        req.updated_after_ms,
        llm_worker_name,
    );

    let args = super::wrap_workflow_run_args(&input);

    let handle = state.jobworkerp().await?;
    let job_id = req
        .dispatch_id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("reflection-{}", chrono::Utc::now().timestamp_millis()));
    let job_id_ret = job_id.clone();

    let entry = state.dispatch_register(&job_id).await;
    let cancel = entry.token.clone();
    let current_job_id = entry.current_job_id.clone();

    tokio::spawn(async move {
        let mut last_progress: Option<(i64, i64)> = None;
        let job_id_for_emit = job_id.clone();
        let app_for_emit = app.clone();
        let park_job_id = current_job_id.clone();
        let cancel_for_emit = cancel.clone();
        run_cancellable_named_stream(
            &handle,
            REFLECTION_WORKER_NAME,
            args,
            Some("run"),
            cancel.clone(),
            move |jid| async move {
                *park_job_id.lock().await = Some(jid);
            },
            move |ev| {
                let (status, message) = match ev {
                    StreamEvent::Active(msg) => {
                        let digest = msg.map(|raw| {
                            if thread_reflection_single_completed(raw) {
                                emit_generated_refresh(
                                    &app_for_emit,
                                    &job_id_for_emit,
                                    vec![GeneratedRefreshScope::Reflection],
                                );
                            }
                            let (d, p) = summarize_workflow_chunk(raw, last_progress);
                            last_progress = p;
                            d
                        });
                        (StepStatus::Active, digest)
                    }
                    StreamEvent::Done(msg) => {
                        emit_generated_refresh(
                            &app_for_emit,
                            &job_id_for_emit,
                            vec![GeneratedRefreshScope::Reflection],
                        );
                        let digest = msg.map(|raw| summarize_workflow_chunk(raw, last_progress).0);
                        (StepStatus::Done, digest)
                    }
                    StreamEvent::Failed(msg) => {
                        let text = if cancel_for_emit.is_cancelled() {
                            "中断".to_string()
                        } else {
                            summarize_workflow_error(msg)
                        };
                        (StepStatus::Failed, Some(text))
                    }
                };
                emit_event(
                    &app_for_emit,
                    REFLECTION_EVENT,
                    ReflectionStepUpdate {
                        job_id: job_id_for_emit.clone(),
                        status,
                        message,
                    },
                );
            },
        )
        .await;
        *current_job_id.lock().await = None;
        if let Some(state) = app.try_state::<AppState>() {
            state.dispatch_take(&job_id).await;
        }
    });

    Ok(EnqueueReflectionJobResponse {
        job_id_hint: job_id_ret,
    })
}

#[tauri::command]
pub async fn reflection_cancel(state: State<'_, AppState>, dispatch_id: String) -> AppResult<()> {
    cancel_dispatch_inner(&state, &dispatch_id).await
}

fn resolve_reflection_batch_yaml() -> Option<PathBuf> {
    let dir = crate::data::paths::workflows_bundle_dir().ok()?;
    let p = dir
        .join("thread-reflection")
        .join("thread-reflection-batch.yaml");
    p.exists().then_some(p)
}

/// Build the JSON document passed to the workflow runner.
/// `thread-reflection-batch.yaml` requires `user_id`,
/// `memories_grpc_host`, `memories_grpc_port`, `prompt_version`; everything
/// else has YAML-side defaults. `output_language` picks the per-language
/// single worker (`memories-thread-reflection-single-<lang>`). `memories_grpc_tls`
/// is added so a remote HTTPS target dials back over TLS.
fn build_workflow_input(
    user_id: i64,
    callback: &MemoriesCallback,
    output_language: &str,
    prompt_version: &str,
    updated_after_ms: Option<i64>,
    llm_worker_name: &str,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "user_id": user_id,
        "memories_grpc_host": callback.host,
        "memories_grpc_port": callback.port,
        "memories_grpc_tls": callback.tls,
        "output_language": output_language,
        "prompt_version": prompt_version,
        "llm_worker_name": llm_worker_name,
    });
    if let Some(ts) = updated_after_ms {
        value["updated_after_ms"] = serde_json::Value::from(ts);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_callback() -> MemoriesCallback {
        MemoriesCallback {
            host: "127.0.0.1".into(),
            port: 9010,
            tls: false,
        }
    }

    #[test]
    fn reflection_progress_chunk_is_digested_not_raw_json() {
        // Regression: the reflection button used to emit the raw WorkflowResult
        // JSON to the toast (`{"id":..,"output":..,"status":"Running",..}`).
        // It must now reuse the import digest so the progress reads
        // `(8/84) 進捗を更新中` like every other batch dispatch.
        let raw = r#"{"id":"019e5c3b","output":"{\"progress_processed\":8,\"progress_total\":84}","position":"/ROOT/do/4/reflectEach/for/do/0/reportProgress","status":"Running","errorMessage":"{\"progress_processed\":8,\"progress_total\":84}"}"#;
        let (digest, progress) = summarize_workflow_chunk(raw, None);
        assert_eq!(digest, "(8/84) 進捗を更新中");
        assert_eq!(progress, Some((8, 84)));
        // The verbatim WorkflowResult envelope must not leak into the toast.
        assert!(!digest.contains("\"id\""));
        assert!(!digest.contains("\"status\""));
    }

    #[test]
    fn default_prompt_version_is_the_shared_reflexion_constant() {
        // The manual dispatch falls back to the shared version constant when
        // the caller omits `prompt_version`. Pin that the default is the
        // bumped Reflexion version (not the legacy "v1"), so regeneration
        // produces fresh reflections under the new reflector prompt rather
        // than being short-circuited by the idempotency key.
        assert_eq!(REFLECTION_PROMPT_VERSION, "20260525-reflexion");
        assert_ne!(REFLECTION_PROMPT_VERSION, "v1");
    }

    #[test]
    fn workflow_input_includes_required_fields() {
        let v = build_workflow_input(1, &dummy_callback(), "ja", "v1", None, "memories-llm");
        assert_eq!(v["user_id"], 1);
        assert_eq!(v["memories_grpc_host"], "127.0.0.1");
        assert_eq!(v["memories_grpc_port"], 9010);
        assert_eq!(v["memories_grpc_tls"], false);
        // The batch resolves `memories-thread-reflection-single-<lang>` by name
        // from `output_language`; the single-yaml path relay is gone.
        assert!(v.get("single_workflow_path").is_none());
        assert_eq!(v["output_language"], "ja");
        assert_eq!(v["prompt_version"], "v1");
        assert_eq!(v["llm_worker_name"], "memories-llm");
        assert!(v.get("updated_after_ms").is_none());
    }

    #[test]
    fn workflow_input_carries_updated_after_when_set() {
        let v = build_workflow_input(
            42,
            &dummy_callback(),
            "ja",
            "v2",
            Some(1_700_000_000_000),
            "memories-llm",
        );
        assert_eq!(v["user_id"], 42);
        assert_eq!(v["prompt_version"], "v2");
        assert_eq!(v["updated_after_ms"], 1_700_000_000_000_i64);
    }

    #[test]
    fn dispatch_wraps_input_into_workflow_run_args() {
        // Regression: the manual dispatch must produce the same
        // `{ input: <json string> }` wire shape as the import pipeline.
        // A bare object would be dropped by enqueue, failing schema
        // validation on the DIRECT-stream batch worker.
        let input = build_workflow_input(
            7,
            &dummy_callback(),
            "ja",
            "v1",
            Some(1_700_000_000_000),
            "memories-llm",
        );
        let args = super::super::wrap_workflow_run_args(&input);

        let obj = args.as_object().expect("args is an object");
        assert_eq!(obj.len(), 1, "only the `input` field is sent");
        let input_str = obj["input"].as_str().expect("input is a JSON string");
        let parsed: serde_json::Value = serde_json::from_str(input_str).unwrap();
        assert_eq!(parsed, input);
        assert_eq!(parsed["user_id"], 7);
        assert_eq!(parsed["updated_after_ms"], 1_700_000_000_000_i64);
    }

    #[test]
    fn workflow_input_propagates_remote_tls() {
        let v = build_workflow_input(
            1,
            &MemoriesCallback {
                host: "memories.example.com".into(),
                port: 8443,
                tls: true,
            },
            "ja",
            "v1",
            None,
            "memories-llm-external",
        );
        assert_eq!(v["memories_grpc_host"], "memories.example.com");
        assert_eq!(v["memories_grpc_port"], 8443);
        assert_eq!(v["memories_grpc_tls"], true);
    }

    /// Drift guard for the reflection lang-worker's multilingual contract.
    ///
    /// The Japanese-output rule used to live inline in the single YAML; it now
    /// moved into the per-language prompt context (`prompts/<role>.ja.txt`),
    /// which `memories-import upsert-generation-workers` bakes into the
    /// `memories-thread-reflection-single-ja` worker's `workflow_context`. The
    /// single YAML must (a) reference that context (not hardcode a prompt) and
    /// (b) fail closed if the context is empty (a registration mistake). The ja
    /// prompt file must carry the Japanese directive for the free-text fields.
    /// This guards the load-bearing pieces so a future edit that drops the
    /// context wiring or the Japanese rule fails CI rather than silently
    /// regenerating off-language reflections.
    #[test]
    fn reflection_lang_worker_keeps_multilingual_contract() {
        let lang_root = crate::data::paths::lang_workers_repo_root()
            .expect("lang-workers repo root resolves in dev");
        let single_path = lang_root
            .join("workers")
            .join("thread-reflection")
            .join("thread-reflection-single.yaml");
        let single = std::fs::read_to_string(&single_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", single_path.display()));

        // (a) The LLM step reads the language-specific prompt from context, and
        //     the LLM call stays on the named worker (local llama-cpp), NOT a
        //     hardcoded prompt or an inline ollama runner.
        assert!(
            single.contains("$reflection_system_prompt"),
            "single must reference the baked system_prompt context"
        );
        assert!(
            single.contains("reflection_user_tail"),
            "single must reference the baked user_tail context"
        );
        assert!(
            single.contains("workerName: \"${$workflow.input.llm_worker_name}\""),
            "reflection single must keep the memories-llm worker call"
        );
        assert!(
            !single.contains("runnerName: LLM"),
            "reflection single must not inline an ollama LLM runner"
        );
        // (b) Fail closed when the context is missing (registration mistake).
        assert!(
            single.contains("prompt_context_missing"),
            "single must fail closed on empty prompt context"
        );

        // The ja prompt carries the Japanese directive naming the free-text
        // fields. Pin a couple so a partial edit dropping one is caught.
        let ja_system = std::fs::read_to_string(
            lang_root
                .join("workers")
                .join("thread-reflection")
                .join("prompts")
                .join("system_prompt.ja.txt"),
        )
        .expect("ja reflection system prompt exists");
        assert!(
            ja_system.contains("日本語"),
            "ja system prompt must require Japanese output"
        );
        for field in ["summary", "task_intent"] {
            assert!(
                ja_system.contains(field),
                "ja system prompt must name the {field} free-text field"
            );
        }
    }
}
