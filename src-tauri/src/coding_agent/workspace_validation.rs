use super::{
    ValidationEvidence, ValidationState,
    model::{ApprovalAssessmentCommand, ApprovedValidationDefinition},
    one_shot_workflow::{OneShotWorkflowError, OneShotWorkflowExecution, OneShotWorkflowResult},
};
use crate::jobworkerp::{JobworkerpHandle, coding_agent::NativeWorkerMatch};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const MAX_VALIDATION_TIMEOUT_MS: u64 =
    super::preparation_workflow::MAX_WORKFLOW_TIMEOUT_SECONDS as u64 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceValidationObservation {
    pub validation_id: String,
    pub command_digest: String,
    pub workflow_result: OneShotWorkflowResult,
}

pub fn command_digest(command: &ApprovalAssessmentCommand) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(command).expect("validated command is serializable"))
    )
}

pub fn summarize(
    definitions: &[ApprovedValidationDefinition],
    observations: &[WorkspaceValidationObservation],
) -> Result<ValidationEvidence, OneShotWorkflowError> {
    if observations.len() > definitions.len() {
        return Err(OneShotWorkflowError::OutcomeUnknown);
    }
    let mut seen = std::collections::BTreeSet::new();
    for observation in observations {
        let definition = definitions
            .iter()
            .find(|item| item.validation_id == observation.validation_id)
            .ok_or(OneShotWorkflowError::OutcomeUnknown)?;
        let command = definition
            .command
            .as_ref()
            .ok_or(OneShotWorkflowError::OutcomeUnknown)?;
        if !seen.insert(&observation.validation_id)
            || observation.command_digest != command_digest(command)
            || observation
                .workflow_result
                .parent_job_id
                .is_some_and(|id| id <= 0)
            || observation.workflow_result.sandbox_id.is_empty()
        {
            return Err(OneShotWorkflowError::OutcomeUnknown);
        }
    }
    let required = definitions.iter().any(|item| item.required);
    let failed = observations
        .iter()
        .any(|item| item.workflow_result.exit_code != 0);
    let missing = definitions
        .iter()
        .filter(|item| item.required)
        .any(|definition| !seen.contains(&definition.validation_id));
    Ok(ValidationEvidence {
        required,
        state: if failed {
            ValidationState::Failed
        } else if !observations.is_empty() && !missing {
            ValidationState::Passed
        } else {
            ValidationState::NotRun
        },
    })
}

pub(crate) async fn start_workspace_validation(
    handle: &JobworkerpHandle,
    matched: &NativeWorkerMatch,
    command: &ApprovalAssessmentCommand,
    cwd: &str,
) -> Result<OneShotWorkflowExecution, OneShotWorkflowError> {
    if matched.using() != "run" || !cwd.starts_with(&format!("{}/", matched.scope().guest_root())) {
        return Err(OneShotWorkflowError::InvalidRequest);
    }
    let (arguments, timeout) = validation_arguments(command, cwd)?;
    handle
        .revalidate_native_version_probe_match(matched)
        .await
        .map_err(|_| OneShotWorkflowError::WorkerChanged)?;
    let definition = json!({
        "document":{"dsl":"1.0.0-jobworkerp","namespace":"lookback","name":"coding-agent-validation","version":"1.0.0"},
        "do":[{"validateWorkspace":{"timeout":{"after":format!("PT{}S", timeout + 5)},"run":{"function":{
            "workerName":matched.worker_name(),"using":"run","arguments":{
                "command":"${ $workflow.input.arguments.command }","args":"${ $workflow.input.arguments.args }",
                "working_dir":"${ $workflow.input.arguments.working_dir }","timeout_ms":"${ $workflow.input.arguments.timeout_ms }",
                "tty":false,"treat_nonzero_as_error":false
            }
        }}}}],"output":{"as":"${ . }"}
    });
    let stream = super::preparation_workflow::start_workflow(
        handle,
        definition,
        json!({"arguments":arguments}),
        "validation",
        timeout,
    )
    .await
    .map_err(|_| OneShotWorkflowError::OutcomeUnknown)?;
    OneShotWorkflowExecution::from_stream(stream, timeout)
}

fn validation_arguments(
    command: &ApprovalAssessmentCommand,
    cwd: &str,
) -> Result<(Value, u32), OneShotWorkflowError> {
    let valid_path = |path: &str| {
        path.starts_with("/workspace")
            && (path == "/workspace" || path.starts_with("/workspace/"))
            && !path.contains(char::is_control)
            && !path.contains(char::from(92))
            && !path
                .split(char::from(47))
                .skip(1)
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
    };
    if !valid_path(&command.working_directory)
        || !valid_path(cwd)
        || command.argv.is_empty()
        || command.argv[0].is_empty()
        || command.argv.len() > 128
        || command
            .argv
            .iter()
            .any(|arg| arg.len() > 16 * 1024 || arg.contains(char::from(0)))
        || command.argv.iter().map(String::len).sum::<usize>() > 64 * 1024
        || command.timeout_ms == 0
        || command.timeout_ms
            > u64::from(super::preparation_workflow::MAX_WORKFLOW_TIMEOUT_SECONDS) * 1000
    {
        return Err(OneShotWorkflowError::InvalidRequest);
    }
    let guest_cwd = format!(
        "{}{}",
        cwd,
        command
            .working_directory
            .strip_prefix("/workspace")
            .ok_or(OneShotWorkflowError::InvalidRequest)?
    );
    let timeout = u32::try_from(command.timeout_ms.div_ceil(1000))
        .map_err(|_| OneShotWorkflowError::InvalidRequest)?;
    Ok((
        json!({"command":command.argv[0],"args":command.argv[1..],"working_dir":guest_cwd,"timeout_ms":command.timeout_ms,"tty":false,"treat_nonzero_as_error":false}),
        timeout,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding_agent::model::{ApprovalAssessmentCommand, ValidationPermission};
    pub(super) fn command() -> ApprovalAssessmentCommand {
        ApprovalAssessmentCommand {
            argv: vec!["cargo".into(), "test".into(), "literal $(never)".into()],
            working_directory: "/workspace/crate".into(),
            permissions: vec![
                ValidationPermission::ReadWorkspace,
                ValidationPermission::WriteBuildOutput,
            ],
            profile_digest: "a".repeat(64),
            timeout_ms: 30_001,
        }
    }
    #[test]
    fn approved_validation_maps_to_the_prepared_workspace_and_keeps_argv_literal() {
        let (arguments, timeout) =
            validation_arguments(&command(), "/workspace/issue-run").unwrap();
        assert_eq!(arguments["working_dir"], "/workspace/issue-run/crate");
        assert_eq!(arguments["args"][1], "literal $(never)");
        assert_eq!(arguments["timeout_ms"], 30_001);
        assert!(u64::from(timeout) * 1000 >= 30_001);
        assert_eq!(arguments["tty"], false);
        assert_eq!(arguments["treat_nonzero_as_error"], false);
    }
    #[test]
    fn summary_requires_real_bound_results_and_preserves_failures() {
        let definition = ApprovedValidationDefinition {
            validation_id: "V-1".into(),
            required: true,
            command: Some(command()),
        };
        let mut observation = WorkspaceValidationObservation {
            validation_id: "V-1".into(),
            command_digest: command_digest(&command()),
            workflow_result: OneShotWorkflowResult {
                exit_code: 0,
                execution_time_ms: 1,
                sandbox_id: "actual".into(),
                workflow_execution_id: None,
                parent_job_id: None,
            },
        };
        assert_eq!(
            summarize(std::slice::from_ref(&definition), &[])
                .unwrap()
                .state,
            ValidationState::NotRun
        );
        assert_eq!(
            summarize(
                std::slice::from_ref(&definition),
                std::slice::from_ref(&observation)
            )
            .unwrap()
            .state,
            ValidationState::Passed
        );
        observation.workflow_result.exit_code = 7;
        assert_eq!(
            summarize(
                std::slice::from_ref(&definition),
                std::slice::from_ref(&observation)
            )
            .unwrap()
            .state,
            ValidationState::Failed
        );
        assert!(
            summarize(
                std::slice::from_ref(&definition),
                &[observation.clone(), observation.clone()]
            )
            .is_err()
        );
        observation.command_digest = "other".into();
        assert!(
            summarize(
                std::slice::from_ref(&definition),
                std::slice::from_ref(&observation)
            )
            .is_err()
        );
        observation.command_digest = command_digest(&command());
        observation.validation_id = "V-other".into();
        assert!(
            summarize(
                std::slice::from_ref(&definition),
                std::slice::from_ref(&observation)
            )
            .is_err()
        );
    }
    #[test]
    fn literal_argv_allows_empty_arguments_and_multiline_scripts() {
        let mut input = command();
        input.argv = vec![
            "python".into(),
            "-c".into(),
            "print(1)\nprint(2)".into(),
            String::new(),
        ];
        let (arguments, _) = validation_arguments(&input, "/workspace/run").unwrap();
        assert_eq!(arguments["args"][1], "print(1)\nprint(2)");
        assert_eq!(arguments["args"][2], "");
        input.argv[2].push(char::from(0));
        assert!(validation_arguments(&input, "/workspace/run").is_err());
    }
    #[test]
    fn optional_success_is_observed_as_passed_but_missing_required_checks_are_not() {
        let definition = ApprovedValidationDefinition {
            validation_id: "V-1".into(),
            required: false,
            command: Some(command()),
        };
        let observation = WorkspaceValidationObservation {
            validation_id: "V-1".into(),
            command_digest: command_digest(&command()),
            workflow_result: OneShotWorkflowResult {
                exit_code: 0,
                execution_time_ms: 1,
                sandbox_id: "actual".into(),
                workflow_execution_id: None,
                parent_job_id: None,
            },
        };
        assert_eq!(
            summarize(
                std::slice::from_ref(&definition),
                std::slice::from_ref(&observation)
            )
            .unwrap()
            .state,
            ValidationState::Passed
        );
        assert_eq!(
            summarize(std::slice::from_ref(&definition), &[])
                .unwrap()
                .state,
            ValidationState::NotRun
        );
        let missing = ApprovedValidationDefinition {
            validation_id: "V-2".into(),
            required: true,
            command: None,
        };
        assert_eq!(
            summarize(&[definition, missing], &[observation])
                .unwrap()
                .state,
            ValidationState::NotRun
        );
    }
    #[test]
    fn validation_rejects_scope_escape_and_unsupported_timeouts() {
        for directory in [
            "/host",
            "/workspace/../host",
            "/workspace//crate",
            "/workspace/.",
        ] {
            let mut input = command();
            input.working_directory = directory.into();
            assert!(validation_arguments(&input, "/workspace/run").is_err());
        }
        for timeout_ms in [0, 10_800_001, u64::MAX] {
            let mut input = command();
            input.timeout_ms = timeout_ms;
            assert!(validation_arguments(&input, "/workspace/run").is_err());
        }
        let mut input = command();
        input.argv.clear();
        assert!(validation_arguments(&input, "/workspace/run").is_err());
    }
}

#[cfg(all(test, unix))]
mod rpc_tests {
    use super::*;
    use crate::jobworkerp::coding_agent::{
        HostWorkerContext, NativeInteractiveWorkerSpec, NativeWorkspaceBinding,
    };
    use crate::native_terminal::registry::native_interactive_rpc_test_fixture::{
        RpcFixture, start_server,
    };
    use jobworkerp_client::{
        jobworkerp::data::{ResultOutputItem, Trailer, result_output_item::Item},
        proto::JobworkerpProto,
    };
    #[tokio::test]
    async fn validation_dispatches_the_bound_workflow_and_requires_end_and_clean_eof() {
        let fixture = RpcFixture::default();
        fixture.enable_preparation_workflow();
        let server = start_server(fixture.clone()).await;
        let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
        let matched = handle.ensure_native_one_shot_worker(NativeInteractiveWorkerSpec {
            native_profile_id: "profile".into(), repository_id: coding_agent_executor::RepositoryId { host: "forge.example".into(), owner: "team".into(), name: "repo".into() }, use_static: false,
            workspace_binding: NativeWorkspaceBinding { canonical_host_root: "/tmp/workspaces".into(), guest_root: "/workspace".into() },
            runner_settings_json: json!({"vm":{"image":"registry.example/coder@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "cpus":1,"memory_mib":1024,"root_disk_mib":4096,
                "mounts":[{"host_path":"/tmp/workspaces","guest_path":"/workspace","writable":true,"executable":true}],"env":{"HOME":"/home/agent"}},
                "allowed_images":["registry.example/coder@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],"allowed_host_mounts":[{"host_root":"/tmp/workspaces","allow_write":true,"allow_exec":true}],"network":{"enabled":false}}),
            host_context: HostWorkerContext { microsandbox_sdk_home: "/tmp/sdk".into(), backend_config_path: "/tmp/sdk/config.json".into() },
        }).await.unwrap();
        let descriptor = JobworkerpProto::parse_result_schema_descriptor(
            &fixture.workflow_runner_data(),
            Some("run"),
        )
        .unwrap()
        .unwrap();
        let data = |code| {
            ResultOutputItem { item: Some(Item::Data(JobworkerpProto::json_value_to_message(descriptor.clone(), &json!({"id":"actual-workflow", "status":"Completed", "output":json!({"exit":{"exitCode":code,"sandboxId":"actual-sandbox"}}).to_string()}),true,false).unwrap())) }
        };
        let end = || ResultOutputItem {
            item: Some(Item::End(Trailer::default())),
        };
        let command = super::tests::command();
        for code in [0, 7] {
            fixture.set_job_result_items(vec![data(code), end()]);
            let pending =
                start_workspace_validation(&handle, &matched, &command, "/workspace/issue-run")
                    .await
                    .unwrap();
            let id = pending.parent_job_id();
            let observed = pending.finish().await.unwrap();
            assert!(id.is_some_and(|id| id > 0));
            assert_eq!(observed.parent_job_id, id);
            assert_eq!(observed.exit_code, code);
        }
        let request = fixture.job_requests().pop().unwrap();
        let arguments = prost_reflect::DynamicMessage::decode(
            JobworkerpProto::parse_job_args_schema_descriptor(
                &fixture.workflow_runner_data(),
                Some("run"),
            )
            .unwrap()
            .unwrap(),
            request.args.as_slice(),
        )
        .unwrap()
        .serialize(serde_json::value::Serializer)
        .unwrap();
        let input: Value = serde_json::from_str(arguments["input"].as_str().unwrap()).unwrap();
        assert_eq!(input["arguments"]["args"][1], "literal $(never)");
        assert_eq!(
            input["arguments"]["working_dir"],
            "/workspace/issue-run/crate"
        );
        assert!(
            matches!(request.worker, Some(jobworkerp_client::jobworkerp::service::job_request::Worker::WorkerId(id)) if id != *matched.worker_id())
        );
        fixture.set_job_result_items(vec![data(0)]);
        assert!(
            start_workspace_validation(&handle, &matched, &command, "/workspace/issue-run")
                .await
                .unwrap()
                .finish()
                .await
                .is_err()
        );
        fixture.set_job_result_items(vec![data(0), end()]);
        fixture.fail_stream_after_output();
        assert!(
            start_workspace_validation(&handle, &matched, &command, "/workspace/issue-run")
                .await
                .unwrap()
                .finish()
                .await
                .is_err()
        );
        let before = fixture.counts().job_enqueue_calls;
        fixture.mutate_runner_data(|runner| {
            runner
                .method_proto_map
                .as_mut()
                .unwrap()
                .schemas
                .get_mut("run")
                .unwrap()
                .description = Some("changed".into())
        });
        assert!(
            start_workspace_validation(&handle, &matched, &command, "/workspace/issue-run")
                .await
                .is_err()
        );
        assert_eq!(fixture.counts().job_enqueue_calls, before);
        assert_eq!(fixture.counts().enqueue_with_client_stream_calls, 0);
    }
}
