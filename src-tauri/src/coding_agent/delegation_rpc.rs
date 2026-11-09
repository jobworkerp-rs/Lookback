use super::delegation::{
    DelegationPlanRequest, DelegationStatusRequest, read_delegation_status, valid_identifier,
};
use crate::grpc::issue_chat_proto::{
    ExecutionDelegationResponse, GetExecutionRequest, ProposeExecutionRequest,
    execution_delegation_response::Outcome,
    lookback_coding_agent_service_server::LookbackCodingAgentService,
};
use std::sync::Arc;
use tauri::Manager;

const MAX_JSON_BYTES: usize = 64 * 1024;

#[async_trait::async_trait]
#[allow(clippy::double_must_use)] // async_trait's boxed future is already must-use.
pub(crate) trait DelegationBackend: Send + Sync {
    async fn propose(
        &self,
        key: &str,
        request: DelegationPlanRequest,
    ) -> Result<String, tonic::Status>;
    async fn get(&self, request: DelegationStatusRequest) -> Result<String, tonic::Status>;
}

pub(crate) struct AppDelegationBackend(pub tauri::AppHandle);

#[async_trait::async_trait]
impl DelegationBackend for AppDelegationBackend {
    async fn propose(
        &self,
        key: &str,
        request: DelegationPlanRequest,
    ) -> Result<String, tonic::Status> {
        let state = self.0.state::<crate::commands::AppState>();
        state
            .ensure_coding_agent_accepting()
            .map_err(|_| tonic::Status::unavailable("coding-agent admission is closed"))?;
        let settings = state
            .coding_agent_settings
            .load()
            .map_err(|_| tonic::Status::unavailable("coding-agent settings unavailable"))?;
        if !settings
            .profiles
            .iter()
            .any(|profile| profile.profile_id == request.profile_id)
        {
            return Err(tonic::Status::not_found("coding-agent profile not found"));
        }
        let issue =
            crate::commands::issues::get_issue_detail(state.clone(), request.issue_id.clone())
                .await
                .map_err(|_| tonic::Status::unavailable("Issue cannot be read"))?;
        let revision = issue
            .revision
            .parse()
            .map_err(|_| tonic::Status::internal("invalid Issue revision"))?;
        let plan = state
            .coding_agent_run_store
            .create_delegation_plan(key, request, revision, settings.revision)
            .map_err(store_error)?;
        encode_plan_result(&plan)
    }

    async fn get(&self, request: DelegationStatusRequest) -> Result<String, tonic::Status> {
        let state = self.0.state::<crate::commands::AppState>();
        let status =
            read_delegation_status(&state.coding_agent_run_store, &request).map_err(store_error)?;
        encode_result(&serde_json::json!({
            "delegationPlan": status.delegation_plan.as_ref().map(plan_summary),
            "runId": status.run_id,
            "state": status.state,
            "reportReferences": status.report_references,
        }))
    }
}

fn store_error(error: super::store::CodingAgentError) -> tonic::Status {
    use super::store::CodingAgentError;
    match error {
        CodingAgentError::NotFound(_) => {
            tonic::Status::not_found("delegation plan or run not found for this Issue")
        }
        CodingAgentError::Invalid(_) => {
            tonic::Status::invalid_argument("invalid delegation request")
        }
        CodingAgentError::Conflict(_) => {
            tonic::Status::already_exists("request key already belongs to a different proposal")
        }
        _ => tonic::Status::internal("delegation persistence unavailable"),
    }
}

fn encode_result(value: &impl serde::Serialize) -> Result<String, tonic::Status> {
    let json = serde_json::to_string(value)
        .map_err(|_| tonic::Status::internal("cannot encode delegation result"))?;
    if json.len() > MAX_JSON_BYTES {
        return Err(tonic::Status::resource_exhausted(
            "delegation result exceeds the size limit",
        ));
    }
    Ok(json)
}

fn plan_summary(plan: &super::delegation::DelegationPlanRecord) -> serde_json::Value {
    // Human approval reads the full stored prompt in the Issue UI; tool results need only its identity.
    serde_json::json!({
        "delegationPlanId": plan.delegation_plan_id,
        "request": {"issueId":plan.request.issue_id, "profileId":plan.request.profile_id, "purpose":plan.request.purpose},
        "issueRevision": plan.issue_revision,
        "settingsRevision": plan.settings_revision,
        "createdAtMs": plan.created_at_ms,
        "runId": plan.run_id,
    })
}

fn encode_plan_result(
    plan: &super::delegation::DelegationPlanRecord,
) -> Result<String, tonic::Status> {
    encode_result(&plan_summary(plan))
}

#[derive(Clone)]
pub(crate) struct DelegationRpc {
    backend: Arc<dyn DelegationBackend>,
}

impl DelegationRpc {
    pub(crate) fn new(backend: Arc<dyn DelegationBackend>) -> Self {
        Self { backend }
    }
}

fn decode_input<T: serde::de::DeserializeOwned>(input: &str) -> Result<T, tonic::Status> {
    if input.len() > MAX_JSON_BYTES {
        return Err(tonic::Status::invalid_argument(
            "delegation input exceeds the size limit",
        ));
    }
    serde_json::from_str(input)
        .map_err(|_| tonic::Status::invalid_argument("invalid delegation input"))
}

#[tonic::async_trait]
impl LookbackCodingAgentService for DelegationRpc {
    async fn propose_execution(
        &self,
        request: tonic::Request<ProposeExecutionRequest>,
    ) -> Result<tonic::Response<ExecutionDelegationResponse>, tonic::Status> {
        let request = request.into_inner();
        if !valid_identifier(&request.request_key, 512) {
            return Err(tonic::Status::invalid_argument("invalid request key"));
        }
        let input: DelegationPlanRequest = decode_input(&request.input_json)?;
        input.validate().map_err(store_error)?;
        let result = self.backend.propose(&request.request_key, input).await?;
        Ok(tonic::Response::new(ExecutionDelegationResponse {
            outcome: Some(Outcome::Success(result)),
        }))
    }

    async fn get_execution(
        &self,
        request: tonic::Request<GetExecutionRequest>,
    ) -> Result<tonic::Response<ExecutionDelegationResponse>, tonic::Status> {
        let input: DelegationStatusRequest = decode_input(&request.into_inner().input_json)?;
        input.validate().map_err(store_error)?;
        let result = self.backend.get(input).await?;
        Ok(tonic::Response::new(ExecutionDelegationResponse {
            outcome: Some(Outcome::Success(result)),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::issue_chat_proto::{
        GetExecutionRequest, ProposeExecutionRequest, execution_delegation_response::Outcome,
        lookback_coding_agent_service_server::LookbackCodingAgentService,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Backend {
        proposals: AtomicUsize,
        reads: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl DelegationBackend for Backend {
        async fn propose(
            &self,
            key: &str,
            request: DelegationPlanRequest,
        ) -> Result<String, tonic::Status> {
            assert_eq!(key, "chat:message:call");
            self.proposals.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({"delegationPlanId":"pending", "issueId":request.issue_id, "runId":null}).to_string())
        }
        async fn get(&self, _: DelegationStatusRequest) -> Result<String, tonic::Status> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({"runId":null,"state":null,"reportReferences":[]}).to_string())
        }
    }

    #[tokio::test]
    async fn delegation_rpc_rejects_authority_and_returns_only_proposal_or_read_result() {
        let backend = Arc::new(Backend::default());
        let service = DelegationRpc::new(backend.clone());
        let input = serde_json::json!({"issueId":"issue", "profileId":"profile", "purpose":"implementation"});
        for key in ["userConfirmed", "command", "autoDone", "mounts"] {
            let mut invalid = input.clone();
            invalid[key] = serde_json::json!(true);
            assert!(
                service
                    .propose_execution(tonic::Request::new(ProposeExecutionRequest {
                        request_key: "chat:message:call".into(),
                        input_json: invalid.to_string(),
                    }))
                    .await
                    .is_err()
            );
        }
        assert_eq!(backend.proposals.load(Ordering::SeqCst), 0);
        let response = service
            .propose_execution(tonic::Request::new(ProposeExecutionRequest {
                request_key: "chat:message:call".into(),
                input_json: input.to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(matches!(response.outcome, Some(Outcome::Success(_))));
        assert_eq!(backend.proposals.load(Ordering::SeqCst), 1);
        for input in [
            r#"{"issueId":"issue"}"#,
            r#"{"issueId":"issue","runId":"run","command":"shell"}"#,
        ] {
            assert!(
                service
                    .get_execution(tonic::Request::new(GetExecutionRequest {
                        input_json: input.into()
                    }))
                    .await
                    .is_err()
            );
        }
        let response = service
            .get_execution(tonic::Request::new(GetExecutionRequest {
                input_json: r#"{"issueId":"issue","delegationPlanId":"pending"}"#.into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(matches!(response.outcome, Some(Outcome::Success(_))));
        assert_eq!(backend.reads.load(Ordering::SeqCst), 1);
        assert_eq!(backend.proposals.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn delegation_rpc_summary_accepts_escaped_input_near_the_limit_without_echoing_the_prompt() {
        let mut request = DelegationPlanRequest {
            issue_id: "issue".into(),
            profile_id: "profile".into(),
            purpose: super::super::RunPurpose::Implementation,
            prompt: Some("\"".repeat(32 * 1024)),
        };
        while serde_json::to_string(&request).unwrap().len() > MAX_JSON_BYTES {
            request.prompt.as_mut().unwrap().pop();
        }
        request.validate().unwrap();
        let plan = super::super::delegation::DelegationPlanRecord {
            delegation_plan_id: "pending-plan".into(),
            request,
            issue_revision: 1,
            settings_revision: 2,
            created_at_ms: 3,
            run_id: None,
        };
        assert!(serde_json::to_string(&plan).unwrap().len() > MAX_JSON_BYTES);
        let response = encode_plan_result(&plan).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["delegationPlanId"], "pending-plan");
        assert!(parsed["request"].get("prompt").is_none());
        assert!(parsed["runId"].is_null());
        assert!(response.len() < MAX_JSON_BYTES);
    }

    #[tokio::test]
    async fn delegation_rpc_wire_returns_a_pending_plan_and_survives_client_disconnect() {
        use crate::grpc::issue_chat_proto::{
            lookback_coding_agent_service_client::LookbackCodingAgentServiceClient,
            lookback_coding_agent_service_server::LookbackCodingAgentServiceServer,
        };
        let backend = Arc::new(Backend::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let incoming = futures::stream::try_unfold(listener, |listener| async move {
            let (socket, _) = listener.accept().await?;
            Ok::<_, std::io::Error>(Some((socket, listener)))
        });
        let stop = tokio_util::sync::CancellationToken::new();
        let shutdown = stop.clone();
        let server = tonic::transport::Server::builder()
            .add_service(LookbackCodingAgentServiceServer::new(DelegationRpc::new(
                backend.clone(),
            )))
            .serve_with_incoming_shutdown(incoming, shutdown.cancelled_owned());
        let task = tokio::spawn(server);
        let endpoint = format!("http://{address}");
        let mut client = LookbackCodingAgentServiceClient::connect(endpoint.clone())
            .await
            .unwrap();
        let invalid = client.propose_execution(ProposeExecutionRequest {
            request_key:"chat:message:call".into(),
            input_json:r#"{"issueId":"issue","profileId":"profile","purpose":"implementation","userConfirmed":true}"#.into(),
        }).await.unwrap_err();
        assert_eq!(invalid.code(), tonic::Code::InvalidArgument);
        let result =
            client
                .propose_execution(ProposeExecutionRequest {
                    request_key: "chat:message:call".into(),
                    input_json:
                        r#"{"issueId":"issue","profileId":"profile","purpose":"implementation"}"#
                            .into(),
                })
                .await
                .unwrap()
                .into_inner();
        let Some(Outcome::Success(json)) = result.outcome else {
            panic!("expected pending plan");
        };
        assert!(serde_json::from_str::<serde_json::Value>(&json).unwrap()["runId"].is_null());
        drop(client);
        let mut client = LookbackCodingAgentServiceClient::connect(endpoint)
            .await
            .unwrap();
        assert!(
            client
                .get_execution(GetExecutionRequest {
                    input_json: r#"{"issueId":"issue","delegationPlanId":"pending"}"#.into(),
                })
                .await
                .is_ok()
        );
        assert_eq!(backend.proposals.load(Ordering::SeqCst), 1);
        assert_eq!(backend.reads.load(Ordering::SeqCst), 1);
        stop.cancel();
        task.await.unwrap().unwrap();
    }
}
