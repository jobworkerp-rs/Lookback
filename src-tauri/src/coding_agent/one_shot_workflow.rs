use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::jobworkerp::{DispatchStream, JobworkerpHandle, coding_agent::NativeWorkerMatch};
use crate::native_terminal::launch::NativeAgentKind;

#[derive(Debug, Clone, Copy, Error, Eq, PartialEq)]
pub enum OneShotWorkflowError {
    #[error("automatic execution request is invalid")]
    InvalidRequest,
    #[error("automatic worker no longer matches the approved configuration")]
    WorkerChanged,
    #[error("automatic workflow result is unconfirmed")]
    OutcomeUnknown,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct OneShotWorkflowResult {
    pub exit_code: i32,
    pub execution_time_ms: u64,
    pub sandbox_id: String,
    #[serde(default)]
    pub workflow_execution_id: Option<String>,
    #[serde(default)]
    pub parent_job_id: Option<i64>,
}

/// A live CLI probe and registered workflow bound to the same SANDBOX worker.
pub struct VerifiedOneShotRuntime {
    workflow: RegisteredOneShotWorkflow,
    cli_version: String,
    probe_job_id: i64,
}

impl VerifiedOneShotRuntime {
    pub fn workflow(&self) -> &RegisteredOneShotWorkflow {
        &self.workflow
    }
    pub fn cli_version(&self) -> &str {
        &self.cli_version
    }
    pub fn probe_job_id(&self) -> i64 {
        self.probe_job_id
    }
}

pub async fn verify_one_shot_runtime(
    handle: &JobworkerpHandle,
    matched: &NativeWorkerMatch,
    profile: &crate::native_terminal::launch::NativeCliProfile,
    prepared_cwd: &str,
) -> Result<VerifiedOneShotRuntime, OneShotWorkflowError> {
    if matched.using() != "run"
        || matched.scope().image_digest() != profile.runtime_image_digest
        || NativeAgentKind::from_command(&profile.expected_profile_command) != Some(profile.kind)
    {
        return Err(OneShotWorkflowError::InvalidRequest);
    }
    let executable = match profile.kind {
        NativeAgentKind::OpenCode => "opencode",
        NativeAgentKind::Codex => "codex",
        NativeAgentKind::ClaudeCode => "claude",
    };
    let mut probe = handle
        .start_native_agent_version_probe(matched, prepared_cwd, executable, Default::default())
        .await
        .map_err(|_| OneShotWorkflowError::OutcomeUnknown)?;
    let probe_job_id = probe.job_id().value;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;
    let mut complete = false;
    loop {
        match probe.next_event().await {
            Ok(Some(crate::jobworkerp::coding_agent::CodingAgentEvent::Output {
                stream,
                data,
            })) => {
                let target = match stream.as_str() {
                    "stdout" => &mut stdout,
                    "stderr" => &mut stderr,
                    _ => break,
                };
                if data.len() > 64 * 1024 - target.len() {
                    break;
                }
                target.extend_from_slice(&data);
            }
            Ok(Some(crate::jobworkerp::coding_agent::CodingAgentEvent::Exit {
                exit_code: code,
                ..
            })) => exit_code = Some(code),
            Ok(None) => {
                complete = true;
                break;
            }
            Err(_) => break,
        }
    }
    if !complete {
        // A failed probe may still own a job; this does not authorize a retry.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            handle.cancel(*probe.job_id()),
        )
        .await;
    }
    let result = crate::native_terminal::doctor::validate_native_cli_probe_for_profile(
        profile, &stdout, &stderr, exit_code, complete, complete, !complete,
    )
    .map_err(|_| OneShotWorkflowError::OutcomeUnknown)?;
    let workflow = register_one_shot_workflow(handle, matched).await?;
    Ok(VerifiedOneShotRuntime {
        workflow,
        cli_version: result.version().to_owned(),
        probe_job_id,
    })
}

pub async fn run_one_shot_workflow(
    handle: &JobworkerpHandle,
    matched: &NativeWorkerMatch,
    profile_command: &[String],
    prompt: &str,
    prepared_cwd: &str,
    timeout_seconds: u32,
) -> Result<OneShotWorkflowResult, OneShotWorkflowError> {
    start_one_shot_workflow(
        handle,
        matched,
        profile_command,
        prompt,
        prepared_cwd,
        timeout_seconds,
    )
    .await?
    .finish()
    .await
}

pub struct OneShotWorkflowExecution {
    parent_job_id: Option<i64>,
    stream: DispatchStream,
    timeout_seconds: u32,
}

impl OneShotWorkflowExecution {
    pub(crate) fn from_stream(
        stream: DispatchStream,
        timeout_seconds: u32,
    ) -> Result<Self, OneShotWorkflowError> {
        let parent_job_id = match stream.job_id {
            Some(id) if id.value > 0 => Some(id.value),
            Some(_) => return Err(OneShotWorkflowError::OutcomeUnknown),
            None => None,
        };
        Ok(Self {
            parent_job_id,
            stream,
            timeout_seconds,
        })
    }

    pub fn parent_job_id(&self) -> Option<i64> {
        self.parent_job_id
    }

    pub async fn finish(self) -> Result<OneShotWorkflowResult, OneShotWorkflowError> {
        let output =
            super::preparation_workflow::finish_workflow(self.stream, self.timeout_seconds)
                .await
                .map_err(|_| OneShotWorkflowError::OutcomeUnknown)?;
        let mut result = decode_sandbox_exit(&output.output)?;
        result.workflow_execution_id = output.execution_id;
        result.parent_job_id = self.parent_job_id;
        Ok(result)
    }
}

pub struct RegisteredOneShotWorkflow {
    workflow: super::preparation_workflow::RegisteredWorkflow,
    sandbox_worker: NativeWorkerMatch,
}

impl RegisteredOneShotWorkflow {
    pub(crate) fn sandbox_worker(&self) -> &NativeWorkerMatch {
        &self.sandbox_worker
    }

    pub fn worker_id(&self) -> i64 {
        self.workflow.worker_id
    }
    pub fn worker_name(&self) -> &str {
        &self.workflow.name
    }

    pub fn definition_hash(&self) -> &str {
        &self.workflow.definition_hash
    }
    pub fn schema_digests(&self) -> (String, String) {
        self.workflow.observed.schema_digests()
    }
    pub async fn revalidate(&self, handle: &JobworkerpHandle) -> Result<(), OneShotWorkflowError> {
        handle
            .revalidate_native_version_probe_match(&self.sandbox_worker)
            .await
            .map_err(|_| OneShotWorkflowError::WorkerChanged)?;
        self.workflow
            .observed
            .revalidate(handle)
            .await
            .map_err(|_| OneShotWorkflowError::WorkerChanged)
    }

    pub async fn start(
        &self,
        handle: &JobworkerpHandle,
        profile_command: &[String],
        prompt: &str,
        prepared_cwd: &str,
        timeout_seconds: u32,
    ) -> Result<OneShotWorkflowExecution, OneShotWorkflowError> {
        if !prepared_cwd.starts_with(&format!("{}/", self.sandbox_worker.scope().guest_root())) {
            return Err(OneShotWorkflowError::InvalidRequest);
        }
        let arguments = one_shot_arguments(profile_command, prompt, prepared_cwd, timeout_seconds)?;
        handle
            .revalidate_native_version_probe_match(&self.sandbox_worker)
            .await
            .map_err(|_| OneShotWorkflowError::WorkerChanged)?;
        let stream = super::preparation_workflow::start_registered_workflow(
            handle,
            &self.workflow,
            json!({"arguments": arguments}),
            timeout_seconds,
        )
        .await
        .map_err(|_| OneShotWorkflowError::OutcomeUnknown)?;
        OneShotWorkflowExecution::from_stream(stream, timeout_seconds)
    }
}

pub async fn register_one_shot_workflow(
    handle: &JobworkerpHandle,
    matched: &NativeWorkerMatch,
) -> Result<RegisteredOneShotWorkflow, OneShotWorkflowError> {
    if matched.using() != "run" {
        return Err(OneShotWorkflowError::InvalidRequest);
    }
    handle
        .revalidate_native_version_probe_match(matched)
        .await
        .map_err(|_| OneShotWorkflowError::WorkerChanged)?;
    let workflow = super::preparation_workflow::register_workflow(
        handle,
        one_shot_definition(matched.worker_name()),
        "one-shot",
    )
    .await
    .map_err(|_| OneShotWorkflowError::OutcomeUnknown)?;
    Ok(RegisteredOneShotWorkflow {
        workflow,
        sandbox_worker: matched.clone(),
    })
}

pub async fn start_one_shot_workflow(
    handle: &JobworkerpHandle,
    matched: &NativeWorkerMatch,
    profile_command: &[String],
    prompt: &str,
    prepared_cwd: &str,
    timeout_seconds: u32,
) -> Result<OneShotWorkflowExecution, OneShotWorkflowError> {
    if matched.using() != "run"
        || !prepared_cwd.starts_with(&format!("{}/", matched.scope().guest_root()))
    {
        return Err(OneShotWorkflowError::InvalidRequest);
    }
    // Validate all per-run inputs before registering a workflow worker.
    one_shot_arguments(profile_command, prompt, prepared_cwd, timeout_seconds)?;
    register_one_shot_workflow(handle, matched)
        .await?
        .start(
            handle,
            profile_command,
            prompt,
            prepared_cwd,
            timeout_seconds,
        )
        .await
}

fn one_shot_arguments(
    profile_command: &[String],
    prompt: &str,
    prepared_cwd: &str,
    timeout_seconds: u32,
) -> Result<Value, OneShotWorkflowError> {
    if prompt.is_empty()
        || prompt.len() > 48 * 1024
        || prompt.contains('\0')
        || timeout_seconds == 0
        || timeout_seconds > super::preparation_workflow::MAX_WORKFLOW_TIMEOUT_SECONDS
        || !prepared_cwd.starts_with('/')
        || prepared_cwd.chars().any(char::is_control)
        || prepared_cwd.contains('\\')
        || prepared_cwd.contains("//")
        || prepared_cwd
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(OneShotWorkflowError::InvalidRequest);
    }
    let kind = NativeAgentKind::from_command(profile_command)
        .ok_or(OneShotWorkflowError::InvalidRequest)?;
    let (command, args) = match kind {
        NativeAgentKind::OpenCode => ("opencode", vec!["run", "--", prompt]),
        NativeAgentKind::Codex => ("codex", vec!["--no-daemon", "exec", "--", prompt]),
        NativeAgentKind::ClaudeCode => ("claude", vec!["-p", "--", prompt]),
    };
    Ok(json!({
        "command": command, "args": args, "working_dir": prepared_cwd,
        "timeout_ms": u64::from(timeout_seconds) * 1000,
        "tty": false, "treat_nonzero_as_error": false,
    }))
}

fn one_shot_definition(worker_name: &str) -> Value {
    json!({
        "document":{"dsl":"1.0.0-jobworkerp", "namespace":"lookback", "name":"coding-agent-one-shot", "version":"1.0.0"},
        "do":[{"runAgent":{"timeout":{"after":format!("PT{}S", super::preparation_workflow::MAX_WORKFLOW_TIMEOUT_SECONDS + 5)}, "run":{"function":{
            "workerName":worker_name, "using":"run", "arguments":{
                "command":"${ $workflow.input.arguments.command }",
                "args":"${ $workflow.input.arguments.args }",
                "working_dir":"${ $workflow.input.arguments.working_dir }",
                "timeout_ms":"${ $workflow.input.arguments.timeout_ms }",
                "tty":false, "treat_nonzero_as_error":false,
            },
        }}}}],
        "output":{"as":"${ . }"}
    })
}

pub(crate) fn decode_sandbox_exit(
    bytes: &[u8],
) -> Result<OneShotWorkflowResult, OneShotWorkflowError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| OneShotWorkflowError::OutcomeUnknown)?;
    let exit = value
        .get("exit")
        .and_then(Value::as_object)
        .ok_or(OneShotWorkflowError::OutcomeUnknown)?;
    let exit_code = match exit.get("exitCode").or_else(|| exit.get("exit_code")) {
        None => 0,
        Some(value) => value
            .as_i64()
            .and_then(|code| i32::try_from(code).ok())
            .ok_or(OneShotWorkflowError::OutcomeUnknown)?,
    };
    let sandbox_id = exit
        .get("sandboxId")
        .or_else(|| exit.get("sandbox_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control))
        .ok_or(OneShotWorkflowError::OutcomeUnknown)?;
    let execution_time_ms = match exit
        .get("executionTimeMs")
        .or_else(|| exit.get("execution_time_ms"))
    {
        None => 0,
        Some(Value::String(time)) => time
            .parse()
            .map_err(|_| OneShotWorkflowError::OutcomeUnknown)?,
        Some(time) => time.as_u64().ok_or(OneShotWorkflowError::OutcomeUnknown)?,
    };
    Ok(OneShotWorkflowResult {
        exit_code,
        execution_time_ms,
        sandbox_id: sandbox_id.to_owned(),
        workflow_execution_id: None,
        parent_job_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn one_shot_keeps_prompt_literal_and_selects_the_registered_run_method() {
        let prompt = "--help\n日本語 ' \" $(touch /tmp/never)";
        for command in [
            vec!["opencode".into(), "run".into()],
            vec!["codex".into(), "exec".into()],
            vec!["claude".into(), "-p".into()],
        ] {
            let arguments = one_shot_arguments(&command, prompt, "/workspace/run", 120).unwrap();
            if command[0] == "opencode" {
                assert_eq!(arguments["args"], json!(["run", "--", prompt]));
            }
            assert_eq!(
                arguments["args"].as_array().unwrap().last().unwrap(),
                prompt
            );
            assert_eq!(arguments["tty"], false);
            assert!(arguments.get("stdin").is_none());
            assert_eq!(arguments["working_dir"], "/workspace/run");
            let workflow = one_shot_definition("lookback.coding-agent.one-shot.verified");
            let function = &workflow["do"][0]["runAgent"]["run"]["function"];
            assert_eq!(function["using"], "run");
            assert_eq!(
                function["workerName"],
                "lookback.coding-agent.one-shot.verified"
            );
            assert_eq!(
                function["arguments"]["args"],
                "${ $workflow.input.arguments.args }"
            );
            assert!(!workflow.to_string().contains(prompt));
            assert!(function.get("runnerName").is_none());
        }
        assert!(
            one_shot_arguments(&["sh".into(), "-c".into()], prompt, "/workspace/run", 120).is_err()
        );
        assert!(
            one_shot_arguments(
                &["opencode".into(), "run".into()],
                "bad\0prompt",
                "/workspace/run",
                120
            )
            .is_err()
        );
        assert!(
            one_shot_arguments(
                &["opencode".into(), "run".into()],
                prompt,
                "/workspace/../other",
                120
            )
            .is_err()
        );
        assert!(
            one_shot_arguments(
                &["opencode".into(), "run".into()],
                prompt,
                "/workspace/run",
                0
            )
            .is_err()
        );
    }

    #[test]
    fn workflow_task_deadline_covers_the_longest_admitted_agent_execution() {
        let command = vec!["opencode".into(), "run".into()];
        let arguments =
            one_shot_arguments(&command, "Issue prompt", "/workspace/run", 10_800).unwrap();
        let definition = one_shot_definition("verified-worker");
        let after = definition["do"][0]["runAgent"]["timeout"]["after"]
            .as_str()
            .expect("agent task has an explicit deadline");
        let seconds = after
            .strip_prefix("PT")
            .unwrap()
            .strip_suffix('S')
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(seconds * 1000 > arguments["timeout_ms"].as_u64().unwrap());
        assert!(one_shot_arguments(&command, "Issue prompt", "/workspace/run", 10_801).is_err());
    }

    #[test]
    fn final_sandbox_exit_is_required_and_nonzero_is_preserved() {
        for exit_code in [0, 1, 137, -1] {
            let output = json!({"exit":{"exitCode":exit_code,"executionTimeMs":"123","sandboxId":"sandbox-observed"}}).to_string();
            assert_eq!(
                decode_sandbox_exit(output.as_bytes()).unwrap().exit_code,
                exit_code
            );
        }
        assert_eq!(
            decode_sandbox_exit(br#"{"exit":{"sandboxId":"observed"}}"#)
                .unwrap()
                .exit_code,
            0
        );
        for output in [
            json!({}),
            json!({"exit":{}}),
            json!({"output":{"stream":"stdout","data":"dGV4dA=="}}),
            json!({"exit":{"exitCode":"bad","sandboxId":"observed"}}),
            json!({"exit":{"exitCode":2147483648_i64,"sandboxId":"observed"}}),
        ] {
            assert!(decode_sandbox_exit(output.to_string().as_bytes()).is_err());
        }
    }
}
