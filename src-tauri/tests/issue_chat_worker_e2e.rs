//! Opt-in real jobworkerp GRPC runner contract test for chat Issue workers.

use std::{path::Path, process::Stdio, time::Duration};

use lookback_tauri_lib::{
    data::DataPaths,
    grpc::{LOOKBACK_CHAT_ISSUE_DESCRIPTOR_SET, issue_chat_proto as chat},
    jobworkerp::JobworkerpHandle,
};
use tonic::{Request, Response, Status, transport::Server};

#[derive(Clone, Default)]
struct EchoIssueService;

#[tonic::async_trait]
impl chat::lookback_chat_issue_service_server::LookbackChatIssueService for EchoIssueService {
    async fn list_issues(
        &self,
        request: Request<chat::ListIssuesRequest>,
    ) -> Result<Response<chat::ListIssuesResponse>, Status> {
        let input = request.into_inner();
        if input.speech_id != "test-turn" || input.page_size != 1 {
            return Err(Status::invalid_argument(
                "request body did not reach facade",
            ));
        }
        Ok(Response::new(chat::ListIssuesResponse {
            outcome: Some(chat::list_issues_response::Outcome::Success(
                chat::IssueListPage {
                    issues: vec![chat::IssueSummary {
                        issue_id: "issue-1".into(),
                        title: "readable".into(),
                        revision: 5,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )),
        }))
    }

    async fn get_issue(
        &self,
        request: Request<chat::GetIssueRequest>,
    ) -> Result<Response<chat::GetIssueResponse>, Status> {
        let input = request.into_inner();
        if input.speech_id != "test-turn" || input.issue_id != "issue-1" {
            return Err(Status::invalid_argument(
                "request body did not reach facade",
            ));
        }
        Ok(Response::new(chat::GetIssueResponse {
            outcome: Some(chat::get_issue_response::Outcome::Success(
                chat::IssueDetail {
                    issue_id: input.issue_id,
                    revision: 5,
                    ..Default::default()
                },
            )),
        }))
    }

    async fn create_issue(
        &self,
        request: Request<chat::CreateIssueRequest>,
    ) -> Result<Response<chat::IssueChatMutationResponse>, Status> {
        let request = request.into_inner();
        mutation_response(
            request.speech_id,
            request.tool_call_id,
            request.input_json,
            "create",
            "c",
        )
    }
    async fn update_issue(
        &self,
        request: Request<chat::UpdateIssueRequest>,
    ) -> Result<Response<chat::IssueChatMutationResponse>, Status> {
        let request = request.into_inner();
        mutation_response(
            request.speech_id,
            request.tool_call_id,
            request.input_json,
            "update",
            "u",
        )
    }
    async fn cancel_issue(
        &self,
        request: Request<chat::CancelIssueRequest>,
    ) -> Result<Response<chat::IssueChatMutationResponse>, Status> {
        let request = request.into_inner();
        mutation_response(
            request.speech_id,
            request.tool_call_id,
            request.input_json,
            "cancel",
            "x",
        )
    }
}

fn mutation_response(
    speech_id: String,
    call_id: String,
    input_json: String,
    method: &str,
    expected_call_id: &str,
) -> Result<Response<chat::IssueChatMutationResponse>, Status> {
    if speech_id != "test-turn" || call_id != expected_call_id || input_json != "{}" {
        return Err(Status::invalid_argument(
            "request body did not reach facade",
        ));
    }
    Ok(Response::new(chat::IssueChatMutationResponse {
        outcome: Some(chat::issue_chat_mutation_response::Outcome::Success(
            chat::IssueChatMutationResult {
                issue_id: format!("issue-{method}"),
                operation_id: "op-1".into(),
                operation_state: "COMMITTED".into(),
                revision: "5".into(),
                ..Default::default()
            },
        )),
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires LOOKBACK_JOBWORKERP_BIN pointing to a built all-in-one"]
async fn real_grpc_worker_round_trips_five_chat_issue_methods() {
    let facade_addr = format!("127.0.0.1:{}", reserve_port());
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(LOOKBACK_CHAT_ISSUE_DESCRIPTOR_SET)
        .build_v1()
        .unwrap();
    let server_addr = facade_addr.parse().unwrap();
    let facade = tokio::spawn(async move {
        Server::builder()
            .add_service(reflection)
            .add_service(
                chat::lookback_chat_issue_service_server::LookbackChatIssueServiceServer::new(
                    EchoIssueService,
                ),
            )
            .serve(server_addr)
            .await
            .unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let data = DataPaths::with_root(root.path());
    data.ensure().unwrap();
    let port = reserve_port();
    let mut child = tokio::process::Command::new(std::env::var("LOOKBACK_JOBWORKERP_BIN").unwrap())
        .current_dir(root.path())
        .env("STORAGE_TYPE", "Standalone")
        .env("STORAGE_RESTORE_AT_STARTUP", "false")
        .env("GRPC_ADDR", format!("127.0.0.1:{port}"))
        .env("SQLITE_URL", data.sqlite_url())
        .env("MCP_ENABLED", "false")
        .env("WORKER_CHANNELS", "default,issue_tracker")
        .env("WORKER_CHANNEL_CONCURRENCIES", "2,2")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let handle = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(handle) = JobworkerpHandle::connect(&url).await {
                break handle;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("jobworkerp becomes ready");
    let yaml = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../workers/lookback-issue-chat-workers.yaml"),
    )
    .unwrap()
    .replace("%{LOOKBACK_ISSUE_CHAT_WORKER_HOST}", "127.0.0.1")
    .replace(
        "%{LOOKBACK_ISSUE_CHAT_WORKER_PORT}",
        facade_addr.rsplit(':').next().unwrap(),
    );
    let yaml_path = root.path().join("issue-chat-workers.yaml");
    std::fs::write(&yaml_path, yaml).unwrap();
    let registered = handle
        .register_workers_from_yaml(&yaml_path)
        .await
        .expect("register five GRPC workers");
    let function_set_yaml = root.path().join("issue-chat-function-set.yaml");
    std::fs::write(&function_set_yaml, "defaults:\n  category: 0\nfunction_sets:\n  - name: issue-chat-contract-test\n    targets:\n      - type: WORKER\n        name: lookback_issue_list\n        using: unary\n      - type: WORKER\n        name: lookback_issue_get\n        using: unary\n      - type: WORKER\n        name: lookback_issue_create\n        using: unary\n      - type: WORKER\n        name: lookback_issue_update\n        using: unary\n      - type: WORKER\n        name: lookback_issue_cancel\n        using: unary\n").unwrap();
    handle
        .register_function_sets_from_yaml(&function_set_yaml)
        .await
        .expect("register Issue FunctionSet");
    let tools: serde_json::Value = serde_json::from_str(
        &handle
            .fetch_function_set_as_tools_json("issue-chat-contract-test")
            .await
            .unwrap(),
    )
    .unwrap();
    let names: Vec<_> = tools
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert_eq!(names.len(), 5);
    for name in [
        "lookback_issue_list",
        "lookback_issue_get",
        "lookback_issue_create",
        "lookback_issue_update",
        "lookback_issue_cancel",
    ] {
        assert!(names.contains(&name), "missing {name}: {names:?}");
    }
    for (name, payload, expected) in [
        (
            "lookback_issue_list",
            serde_json::json!({"pageSize":1,"speechId":"test-turn"}),
            "issue-1",
        ),
        (
            "lookback_issue_get",
            serde_json::json!({"issueId":"issue-1","speechId":"test-turn"}),
            "issue-1",
        ),
        (
            "lookback_issue_create",
            serde_json::json!({"speechId":"test-turn","toolCallId":"c","inputJson":"{}"}),
            "issue-create",
        ),
        (
            "lookback_issue_update",
            serde_json::json!({"speechId":"test-turn","toolCallId":"u","inputJson":"{}"}),
            "issue-update",
        ),
        (
            "lookback_issue_cancel",
            serde_json::json!({"speechId":"test-turn","toolCallId":"x","inputJson":"{}"}),
            "issue-cancel",
        ),
    ] {
        assert!(
            registered.contains_key(name),
            "worker not registered: {name}"
        );
        let result = handle
            .dispatch_unary(
                name,
                serde_json::json!({"jsonBody": payload.to_string()}),
                Some("unary"),
            )
            .await
            .expect("dispatch worker");
        assert_eq!(
            result
                .get("code")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0),
            0,
            "{name}: {result}"
        );
        let body: serde_json::Value =
            serde_json::from_str(result["jsonBody"].as_str().expect("JSON response body")).unwrap();
        let issue_id = if name == "lookback_issue_list" {
            &body["success"]["issues"][0]["issueId"]
        } else {
            &body["success"]["issueId"]
        };
        assert_eq!(issue_id, expected, "{name}: {body}");
    }
    // Exercise the persistent registry and the warmed FunctionSet name cache
    // before removing the app-owned Issue workers on the same live process.
    let unrelated_yaml = root.path().join("unrelated-worker.yaml");
    std::fs::write(
        &unrelated_yaml,
        std::fs::read_to_string(&yaml_path).unwrap().replace(
            "name: lookback_issue_list\n",
            "name: lookback_unrelated_echo\n",
        ),
    )
    .unwrap();
    handle
        .register_workers_from_yaml(&unrelated_yaml)
        .await
        .unwrap();
    std::fs::write(
        &function_set_yaml,
        "defaults:\n  category: 0\nfunction_sets:\n  - name: issue-chat-contract-test\n    targets:\n      - type: WORKER\n        name: lookback_unrelated_echo\n        using: unary\n",
    )
    .unwrap();
    handle
        .register_function_sets_from_yaml(&function_set_yaml)
        .await
        .unwrap();
    let updated: serde_json::Value = serde_json::from_str(
        &handle
            .fetch_function_set_as_tools_json("issue-chat-contract-test")
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(updated.as_array().unwrap().len(), 1);
    assert_eq!(updated[0]["function"]["name"], "lookback_unrelated_echo");
    for name in registered.keys() {
        handle.delete_worker_by_name_if_present(name).await.unwrap();
        handle.delete_worker_by_name_if_present(name).await.unwrap();
        assert!(
            handle.worker_id_by_name(name).await.unwrap().is_none(),
            "{name}"
        );
        assert!(
            handle
                .dispatch_unary(name, serde_json::json!({}), Some("unary"))
                .await
                .is_err(),
            "deleted worker {name} must reject direct calls"
        );
    }
    assert!(
        handle
            .worker_id_by_name("lookback_unrelated_echo")
            .await
            .unwrap()
            .is_some()
    );
    let _ = child.kill().await;
    let _ = child.wait().await;
    facade.abort();
}

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
