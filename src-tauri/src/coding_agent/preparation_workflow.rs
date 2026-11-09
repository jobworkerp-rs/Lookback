use std::path::Path;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::jobworkerp::{
    DispatchStream, JobworkerpHandle, ListenChunk, ProgressEvent, WORKFLOW_RESULT_MESSAGE,
    render_message_with_defaults,
};

use super::executor_client::{MAX_RESPONSE_BYTES, NativeExecutorError};

pub(crate) const MAX_WORKFLOW_TIMEOUT_SECONDS: u32 = 10_800;

pub(crate) async fn execute_preparation_workflow(
    handle: &JobworkerpHandle,
    executable: &Path,
    config: &Path,
    input: &[u8],
    response_limit: usize,
) -> Result<Vec<u8>, NativeExecutorError> {
    let definition = preparation_definition(executable, config)?;
    let request = std::str::from_utf8(input).map_err(|_| NativeExecutorError::InvalidRequest)?;
    let stream = start_workflow(
        handle,
        definition,
        json!({"executor_request_json":request}),
        "prepare",
        150,
    )
    .await?;
    finish_workflow_with_limit(stream, 150, response_limit)
        .await
        .map(|result| result.output)
}

pub(crate) async fn start_workflow(
    handle: &JobworkerpHandle,
    definition: Value,
    input: Value,
    route: &str,
    timeout_seconds: u32,
) -> Result<DispatchStream, NativeExecutorError> {
    if input.to_string().len() > MAX_RESPONSE_BYTES
        || timeout_seconds == 0
        || timeout_seconds > MAX_WORKFLOW_TIMEOUT_SECONDS
    {
        return Err(NativeExecutorError::InvalidRequest);
    }
    let registered = register_workflow(handle, definition, route).await?;
    start_registered_workflow(handle, &registered, input, timeout_seconds).await
}

pub(crate) struct RegisteredWorkflow {
    pub(crate) name: String,
    pub(crate) worker_id: i64,
    pub(crate) observed: crate::jobworkerp::workflow::VerifiedRegisteredWorker,
    pub(crate) definition_hash: String,
}

pub(crate) async fn register_workflow(
    handle: &JobworkerpHandle,
    definition: Value,
    route: &str,
) -> Result<RegisteredWorkflow, NativeExecutorError> {
    let definition_json = definition.to_string();
    let name = format!(
        "lookback.coding-agent.{route}.{:x}",
        Sha256::digest(definition_json.as_bytes())
    );
    let workers = json!({
        "defaults": {"response_type":"DIRECT", "store_failure":true, "broadcast_results":false},
        "workers": [{"name":name, "runner":"WORKFLOW", "settings":{"workflow_data":definition_json}}],
    });
    let registered = handle
        .register_workers_from_yaml_str(&workers.to_string(), Path::new("/"))
        .await
        .map_err(|_| NativeExecutorError::Unavailable)?;
    let worker_id = registered
        .get(&name)
        .map(|id| id.value)
        .filter(|id| *id > 0)
        .ok_or(NativeExecutorError::Unavailable)?;
    let observed = handle
        .observe_registered_workflow(&name, worker_id, &definition)
        .await
        .map_err(|_| NativeExecutorError::Unavailable)?;
    Ok(RegisteredWorkflow {
        definition_hash: format!("{:x}", Sha256::digest(definition_json.as_bytes())),
        name,
        worker_id,
        observed,
    })
}

pub(crate) async fn start_registered_workflow(
    handle: &JobworkerpHandle,
    registered: &RegisteredWorkflow,
    input: Value,
    timeout_seconds: u32,
) -> Result<DispatchStream, NativeExecutorError> {
    start_registered_workflow_with_cancellation(handle, registered, input, timeout_seconds, None)
        .await
}

pub(crate) async fn start_registered_workflow_with_cancellation(
    handle: &JobworkerpHandle,
    registered: &RegisteredWorkflow,
    input: Value,
    timeout_seconds: u32,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> Result<DispatchStream, NativeExecutorError> {
    if input.to_string().len() > MAX_RESPONSE_BYTES
        || timeout_seconds == 0
        || timeout_seconds > MAX_WORKFLOW_TIMEOUT_SECONDS
    {
        return Err(NativeExecutorError::InvalidRequest);
    }
    handle
        .dispatch_verified_workflow_with_cancellation(
            &registered.observed,
            json!({"input":input.to_string()}),
            u64::from(timeout_seconds),
            cancellation,
        )
        .await
        .map_err(|_| unknown())
}

pub(crate) struct WorkflowCompletionResult {
    pub(crate) output: Vec<u8>,
    pub(crate) execution_id: Option<String>,
}

pub(crate) async fn finish_workflow(
    stream: DispatchStream,
    timeout_seconds: u32,
) -> Result<WorkflowCompletionResult, NativeExecutorError> {
    finish_workflow_with_limit(stream, timeout_seconds, MAX_RESPONSE_BYTES).await
}

async fn finish_workflow_with_limit(
    stream: DispatchStream,
    timeout_seconds: u32,
    response_limit: usize,
) -> Result<WorkflowCompletionResult, NativeExecutorError> {
    if !matches!(
        response_limit,
        MAX_RESPONSE_BYTES | coding_agent_executor::MAX_SOURCE_RESPONSE_BYTES
    ) {
        return Err(NativeExecutorError::InvalidRequest);
    }
    let descriptor = stream
        .result_descriptor()
        .filter(|desc| desc.full_name() == WORKFLOW_RESULT_MESSAGE)
        .ok_or_else(unknown)?;
    let mut result = WorkflowCompletion {
        response_limit,
        ..WorkflowCompletion::default()
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(u64::from(timeout_seconds) + 5),
        stream.drain_confirmed_bytes(|event| {
            let (bytes, ended) = match event {
                ListenChunk::Data(bytes) => (Some(bytes), false),
                ListenChunk::Final { collected } => (collected, true),
            };
            let has_payload = bytes.is_some();
            let decoded = bytes.and_then(|bytes| {
                if bytes.len() > response_limit * 8 {
                    return None;
                }
                render_message_with_defaults(&bytes, &descriptor)
            });
            if has_payload && decoded.is_none() {
                result.invalid = true;
            }
            if ended {
                result.observe(ProgressEvent::End {
                    final_text: decoded,
                });
            } else if let Some(text) = decoded {
                result.observe(ProgressEvent::Chunk { text });
            } else {
                result.invalid = true;
            }
        }),
    )
    .await
    .map_err(|_| unknown())?
    .map_err(|_| unknown())?;
    result.finish()
}

pub(crate) fn preparation_definition(
    executable: &Path,
    config: &Path,
) -> Result<Value, NativeExecutorError> {
    let executable = executable
        .to_str()
        .ok_or(NativeExecutorError::InvalidPackage)?;
    let config = config
        .to_str()
        .ok_or(NativeExecutorError::InvalidTrustedConfiguration)?;
    Ok(json!({
        "document":{"dsl":"1.0.0-jobworkerp", "namespace":"lookback", "name":"coding-agent-prepare", "version":"1.0.0"},
        "input":{"schema":{"document":{"type":"object", "required":["executor_request_json"], "properties":{"executor_request_json":{"type":"string"}}}}},
        "do":[{"invokeExecutor":{"run":{"function":{"runnerName":"COMMAND", "settings":{}, "arguments":{
            "command":executable, "args":["--config",config,"--stdio"],
            "stdin":"${ $workflow.input.executor_request_json }", "treat_nonzero_as_error":true
        }}}}}],
        "output":{"as":"${ .stdout }"}
    }))
}

struct WorkflowCompletion {
    latest: Option<String>,
    ended: bool,
    invalid: bool,
    response_limit: usize,
}

impl Default for WorkflowCompletion {
    fn default() -> Self {
        Self {
            latest: None,
            ended: false,
            invalid: false,
            response_limit: MAX_RESPONSE_BYTES,
        }
    }
}

impl WorkflowCompletion {
    fn observe(&mut self, event: ProgressEvent) {
        let next = match event {
            ProgressEvent::Chunk { text } => Some(text),
            ProgressEvent::End { final_text } => {
                self.ended = true;
                final_text
            }
        };
        if let Some(text) = next {
            if text.len() > self.response_limit * 8 {
                self.invalid = true;
                self.latest = None;
            } else {
                self.latest = Some(text);
            }
        }
    }

    fn finish(self) -> Result<WorkflowCompletionResult, NativeExecutorError> {
        if !self.ended || self.invalid {
            return Err(unknown());
        }
        let text = self.latest.ok_or_else(unknown)?;
        let result: Value = serde_json::from_str(&text).map_err(|_| unknown())?;
        let completed = result
            .get("status")
            .is_some_and(|status| status == "Completed" || status == 0);
        let no_error = ["errorMessage", "error_message"].into_iter().all(|key| {
            result
                .get(key)
                .is_none_or(|error| error.is_null() || error.as_str() == Some(""))
        });
        let output = result
            .get("output")
            .and_then(Value::as_str)
            .ok_or_else(unknown)?;
        if !completed || !no_error || output.is_empty() || output.len() > self.response_limit * 8 {
            return Err(unknown());
        }
        // WORKFLOW output serializes strings as JSON; preserve the helper's JSON-line payload.
        let mut output =
            serde_json::from_str::<String>(output).unwrap_or_else(|_| output.to_owned());
        // COMMAND joins captured lines without the helper protocol's trailing newline.
        if !output.ends_with('\n') {
            output.push('\n');
        }
        if output.len() > self.response_limit {
            return Err(unknown());
        }
        let execution_id = match result.get("id") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) if id.is_empty() => None,
            Some(Value::String(id)) if id.len() <= 512 && !id.chars().any(char::is_control) => {
                Some(id.clone())
            }
            _ => return Err(unknown()),
        };
        Ok(WorkflowCompletionResult {
            output: output.into_bytes(),
            execution_id,
        })
    }
}

fn unknown() -> NativeExecutorError {
    NativeExecutorError::OutcomeUnknown {
        operation: "preparation_workflow",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn workflow_identity_is_observed_from_the_completed_result_and_remains_optional() {
        for (identity, expected) in [
            (None, None),
            (Some(json!(null)), None),
            (Some(json!("")), None),
            (
                Some(json!("actual-workflow-id")),
                Some("actual-workflow-id"),
            ),
        ] {
            let mut payload = json!({"status":"Completed", "output":"actual output"});
            if let Some(identity) = identity {
                payload["id"] = identity;
            }
            let mut result = WorkflowCompletion::default();
            result.observe(ProgressEvent::Chunk {
                text: payload.to_string(),
            });
            result.observe(ProgressEvent::End { final_text: None });
            assert_eq!(result.finish().unwrap().execution_id.as_deref(), expected);
        }
        for identity in [json!(17), json!("bad\nidentity"), json!("x".repeat(513))] {
            let mut result = WorkflowCompletion::default();
            result.observe(ProgressEvent::End {
                final_text: Some(
                    json!({"id":identity, "status":"Completed", "output":"actual output"})
                        .to_string(),
                ),
            });
            assert!(result.finish().is_err());
        }
    }

    #[test]
    fn completed_result_requires_real_end_and_a_successful_workflow() {
        let response = "{\"protocol_version\":2,\"status\":\"ok\"}";
        let completed = json!({"status":"Completed","output":response}).to_string();
        let mut result = WorkflowCompletion::default();
        result.observe(ProgressEvent::Chunk {
            text: completed.clone(),
        });
        assert!(result.finish().is_err());
        let mut result = WorkflowCompletion::default();
        result.observe(ProgressEvent::Chunk { text: completed });
        result.observe(ProgressEvent::End { final_text: None });
        assert_eq!(
            result.finish().unwrap().output,
            format!("{response}\n").as_bytes()
        );
        for status in ["Running", "Faulted", "Cancelled", "Pending", "Waiting"] {
            let mut result = WorkflowCompletion::default();
            result.observe(ProgressEvent::End {
                final_text: Some(json!({"status":status,"output":response}).to_string()),
            });
            assert!(result.finish().is_err(), "{status}");
        }
    }

    #[test]
    fn final_collected_replaces_progress_and_rejects_error_or_oversized_output() {
        let mut result = WorkflowCompletion::default();
        result.observe(ProgressEvent::Chunk {
            text: "not the final result".into(),
        });
        result.observe(ProgressEvent::End {
            final_text: Some(json!({"status":"Completed","output":"{}"}).to_string()),
        });
        assert_eq!(result.finish().unwrap().output, b"{}\n");
        for final_result in [
            json!({"status":"Completed","output":"{}","errorMessage":"failed"}),
            json!({"status":"Completed","output":"x".repeat(MAX_RESPONSE_BYTES + 1)}),
            json!({"output":"{}"}),
        ] {
            let mut result = WorkflowCompletion::default();
            result.observe(ProgressEvent::End {
                final_text: Some(final_result.to_string()),
            });
            assert!(result.finish().is_err());
        }
    }

    #[test]
    fn workflow_output_limit_applies_to_decoded_payload_not_json_escaping() {
        let payload = format!(
            "{{\"value\":\"{}\"}}",
            "\\\\".repeat(MAX_RESPONSE_BYTES / 4)
        );
        assert!(payload.len() < MAX_RESPONSE_BYTES);
        let encoded = serde_json::to_string(&payload).unwrap();
        assert!(encoded.len() > MAX_RESPONSE_BYTES);
        let mut result = WorkflowCompletion::default();
        result.observe(ProgressEvent::End {
            final_text: Some(json!({"status":"Completed","output":encoded}).to_string()),
        });
        assert_eq!(
            result.finish().unwrap().output,
            format!("{payload}\n").as_bytes()
        );
    }

    #[test]
    fn preparation_definition_uses_literal_host_paths_and_request_stdin() {
        let definition = preparation_definition(
            Path::new("/host/package/executor name;$(echo injected)"),
            Path::new("/host/config/settings.json"),
        )
        .unwrap();
        let args = &definition["do"][0]["invokeExecutor"]["run"]["function"]["arguments"];
        assert_eq!(
            args["command"],
            "/host/package/executor name;$(echo injected)"
        );
        assert_eq!(
            args["args"],
            json!(["--config", "/host/config/settings.json", "--stdio"])
        );
        assert_eq!(args["stdin"], "${ $workflow.input.executor_request_json }");
        assert_eq!(definition["output"]["as"], "${ .stdout }");
    }
}
