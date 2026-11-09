//! Contract tests for the registered coding-agent judge provider.

use coding_agent::judge_provider;
use lookback_tauri_lib::{coding_agent, commands, grpc};

use coding_agent::assessment::{
    self as assessment, AssessmentClock, AssessmentEngine, AssessmentOutcome,
    AssessmentPreparation, AssessmentStore, JudgeProvider, JudgeProviderError, JudgeRequest,
    ProviderSelection, ReceiptLookup, prepare_assessment,
};
use commands::llm_settings::{LlmMode, LlmSettings};
use grpc::proto::jobworkerp::runner::llm::{
    LlmChatResult, llm_chat_result::MessageContent, llm_chat_result::message_content::Content,
};
use prost::Message;
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::{Stream, stream};
use jobworkerp_client::{
    command::job::JOB_ID_HEADER_NAME,
    jobworkerp::{
        data::{
            Job, JobId, JobResult, JobResultData, JobResultId, MethodProtoMap, MethodSchema,
            QueueType, ResponseType, ResultOutput, ResultOutputItem, ResultStatus, Runner,
            RunnerData, RunnerId, StreamingOutputType, Trailer, Worker, WorkerData, WorkerId,
            result_output_item::Item,
        },
        service::{
            self, ClientStreamRequest, CountCondition, CountJobResultRequest, CountResponse,
            CountRunnerRequest, CountWorkerRequest, CreateJobResponse, CreateRunnerRequest,
            CreateRunnerResponse, CreateWorkerResponse, DeleteJobResultBulkRequest,
            DeleteJobResultBulkResponse, FindJobResultListRequest, FindListByJobIdRequest,
            FindListRequest, FindListWithProcessingStatusRequest, FindQueueListRequest,
            FindRunnerListRequest, FindWorkerListRequest, JobAndStatus, ListenByWorkerRequest,
            ListenRequest, OptionalJobResponse, OptionalJobResultResponse, OptionalRunnerResponse,
            OptionalWorkerResponse, ReleaseStaticWorkerRequest, RunnerNameRequest, SuccessResponse,
            WorkerNameRequest,
            job_result_service_server::{JobResultService, JobResultServiceServer},
            job_service_server::{JobService, JobServiceServer},
            runner_service_server::{RunnerService, RunnerServiceServer},
            worker_service_server::{WorkerService, WorkerServiceServer},
        },
    },
    proto::JobworkerpProto,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, metadata::MetadataValue, transport::Server};

const REPORT: &str = r#"{
  "schema_version": 1,
  "run_id": "run-judge",
  "attempt_id": "attempt-judge",
  "task_kind": "implementation",
  "status": "completed",
  "summary": "The report is complete — evidence from café.",
  "acceptance": [
    {"criterion_id":"AC-semantic","assessment":"met","evidence":["report:attempt-judge"]}
  ],
  "validation": [],
  "investigation": null,
  "implementation": {
    "change_summary":"Implementation reported.",
    "changed_files":["src/lib.rs"],
    "implementation_notes":[]
  },
  "unresolved": []
}"#;

fn external_settings(model: &str, base_url: Option<&str>) -> LlmSettings {
    LlmSettings {
        mode: LlmMode::External,
        provider_model: Some(model.to_owned()),
        base_url: base_url.map(str::to_owned),
        ..LlmSettings::default()
    }
}

fn runner_settings_proto() -> String {
    // This test-only copy keeps the runner descriptor available in standalone app checkouts.
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/coding_agent_judge_runner_settings.schema.json"
    ))
    .unwrap();
    let lines = fixture["runner_settings_proto_lines"]
        .as_array()
        .expect("runner settings fixture must contain protobuf source lines");
    let source = lines
        .iter()
        .map(|line| {
            line.as_str()
                .expect("runner settings source lines must be strings")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{source}\n")
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn validated_report() -> lookback_tauri_lib::coding_agent::report::ReportReadResult {
    use std::collections::BTreeMap;

    use lookback_tauri_lib::coding_agent::report::{ReportExpectation, TaskKind};

    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let path = workspace.join(
        lookback_tauri_lib::coding_agent::report::report_relative_path("attempt-judge").unwrap(),
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, REPORT.as_bytes()).unwrap();
    let expectation = ReportExpectation::new(
        "run-judge",
        "attempt-judge",
        TaskKind::Implementation,
        BTreeMap::from([("AC-semantic".to_owned(), true)]),
        BTreeMap::new(),
    )
    .unwrap();
    lookback_tauri_lib::coding_agent::report::read_report(&workspace, &expectation)
}

fn prepared_assessment(
    assessment_id: &str,
    deadline_unix_ms: u64,
) -> assessment::PreparedAssessment {
    use lookback_tauri_lib::coding_agent::report::TaskKind;

    let settings = external_settings("gpt-4o-2024-08-06", Some("https://llm.example.test/v1"));
    let provider = judge_provider::provider_selection_for_settings(&settings).unwrap();
    let snapshot = assessment::AssessmentApprovalSnapshot {
        authorization: assessment::AssessmentAuthorization::RunApproval,
        run_id: "run-judge".to_owned(),
        attempt_id: "attempt-judge".to_owned(),
        task_kind: TaskKind::Implementation,
        approval_snapshot_digest: "a".repeat(64),
        frozen_artifact_digest: "b".repeat(64),
        report_digest: assessment::sha256_hex(REPORT.as_bytes()),
        validation_receipts: Vec::new(),
        validation_plan: Vec::new(),
        criteria: vec![assessment::ApprovedCriterion {
            criterion_id: "AC-semantic".to_owned(),
            statement: "The report demonstrates the accepted behavior.".to_owned(),
            required: true,
            method: assessment::CriterionMethod::SemanticJudge,
        }],
        allowed_sources: vec![assessment::AllowedEvidenceSource {
            kind: assessment::EvidenceSourceKind::Report,
            identity: "report:attempt-judge".to_owned(),
            max_bytes: 32 * 1024,
        }],
        provider,
        prompt_policy_version: "coding-agent-judge-v1".to_owned(),
        max_input_bytes: 128 * 1024,
        deadline_unix_ms,
    };
    let policy = assessment::ApprovedAssessmentPolicy::new(snapshot).unwrap();
    match prepare_assessment(&policy, assessment_id, Vec::new(), &validated_report()).unwrap() {
        AssessmentPreparation::Ready(prepared) => *prepared,
        AssessmentPreparation::NotRequired => unreachable!(),
    }
}

fn valid_assessment_response(prepared: &assessment::PreparedAssessment) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "assessment_id": prepared.assessment_id(),
        "run_id": "run-judge",
        "attempt_id": "attempt-judge",
        "input_digest": prepared.judge_input_digest(),
        "criteria": [{
            "criterion_id": "AC-semantic",
            "result": "PASS",
            "reason": "The report supplies the required evidence.",
            "evidence_refs": ["report:attempt-judge"]
        }],
        "overall": "PASS"
    }))
    .unwrap()
}

#[derive(Clone, Copy)]
struct FixedClock(u64);

impl AssessmentClock for FixedClock {
    fn now_unix_ms(&self) -> u64 {
        self.0
    }
}

struct SeedProvider {
    response: Vec<u8>,
    lookup: ReceiptLookup,
}

impl JudgeProvider for SeedProvider {
    fn complete_tool_free(
        &mut self,
        _request: &JudgeRequest<'_>,
    ) -> Result<Vec<u8>, JudgeProviderError> {
        if self.response.is_empty() {
            Err(JudgeProviderError::TransportFailure)
        } else {
            Ok(self.response.clone())
        }
    }

    fn lookup_receipt(
        &mut self,
        _assessment_id: &str,
        _input_digest: &str,
        _idempotency_key: &str,
    ) -> Result<ReceiptLookup, JudgeProviderError> {
        Ok(self.lookup.clone())
    }
}

fn provider_profile(deadline_unix_ms: u64) -> judge_provider::ApprovedJudgeProviderProfile {
    let settings = external_settings("gpt-4o-2024-08-06", Some("https://llm.example.test/v1"));
    let selection = judge_provider::provider_selection_for_settings(&settings).unwrap();
    judge_provider::ApprovedJudgeProviderProfile::from_settings(
        &settings,
        selection,
        deadline_unix_ms,
    )
    .unwrap()
}

async fn wait_until(deadline_unix_ms: u64) {
    let remaining = deadline_unix_ms.saturating_sub(current_unix_ms());
    if remaining > 0 {
        tokio::time::sleep(Duration::from_millis(remaining + 1)).await;
    }
}

async fn offline_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}")
}

const RPC_RUNNER_ID: i64 = 19;
const RPC_WORKER_ID: i64 = 71;
const RPC_JOB_ID: i64 = 843;

type RpcStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RpcDelivery {
    Complete,
    LoseStreamAfterEnqueue,
    CollectedWithoutEnd,
    CompleteWithErrorTrailer,
    CompleteWithTypedErrorTrailer,
}

struct RpcCalls {
    runner: RunnerData,
    worker: Option<Worker>,
    worker_lookups: usize,
    worker_lookup_delay: Duration,
    mutate_worker_on_lookup: Option<usize>,
    created_worker: Option<WorkerData>,
    enqueue_requests: Vec<service::JobRequest>,
    cancelled_jobs: Vec<i64>,
    enqueue_response_gate: Option<Arc<tokio::sync::Notify>>,
    job_result: Option<JobResult>,
    result_lookup: Vec<FindJobResultListRequest>,
    result_lookup_error: bool,
    result_mutation: Option<fn(&mut JobResult)>,
    duplicate_result: bool,
    empty_result_lookups: usize,
    response: Vec<u8>,
    model: String,
    delivery: RpcDelivery,
    result_job_id: i64,
}

#[derive(Clone)]
struct JudgeRpcFixture(Arc<Mutex<RpcCalls>>);

impl JudgeRpcFixture {
    fn new(response: Vec<u8>, model: &str, delivery: RpcDelivery) -> Self {
        let mut schemas = std::collections::HashMap::new();
        schemas.insert(
            "chat".to_owned(),
            MethodSchema {
                args_proto: include_str!("../../proto/jobworkerp/runner/llm/chat_args.proto")
                    .to_owned(),
                result_proto: include_str!("../../proto/jobworkerp/runner/llm/chat_result.proto")
                    .to_owned(),
                description: Some("LLM chat".to_owned()),
                output_type: StreamingOutputType::Streaming as i32,
                require_client_stream: false,
                client_stream_data_proto: None,
            },
        );
        let runner = RunnerData {
            name: "LLM".to_owned(),
            runner_settings_proto: runner_settings_proto(),
            method_proto_map: Some(MethodProtoMap { schemas }),
            ..Default::default()
        };
        Self(Arc::new(Mutex::new(RpcCalls {
            runner,
            worker: None,
            worker_lookups: 0,
            worker_lookup_delay: Duration::ZERO,
            mutate_worker_on_lookup: None,
            created_worker: None,
            enqueue_requests: Vec::new(),
            cancelled_jobs: Vec::new(),
            enqueue_response_gate: None,
            job_result: None,
            result_lookup: Vec::new(),
            result_lookup_error: false,
            result_mutation: None,
            duplicate_result: false,
            empty_result_lookups: 0,
            response,
            model: model.to_owned(),
            delivery,
            result_job_id: RPC_JOB_ID,
        })))
    }

    fn set_worker_mutation(&self, lookup_number: usize) {
        self.0.lock().unwrap().mutate_worker_on_lookup = Some(lookup_number);
    }

    fn set_result_lookup_error(&self) {
        self.0.lock().unwrap().result_lookup_error = true;
    }

    fn set_result_job_id(&self, job_id: i64) {
        self.0.lock().unwrap().result_job_id = job_id;
    }

    fn enqueue_count(&self) -> usize {
        self.0.lock().unwrap().enqueue_requests.len()
    }

    fn result_lookup_count(&self) -> usize {
        self.0.lock().unwrap().result_lookup.len()
    }

    fn captured_worker(&self) -> WorkerData {
        self.0.lock().unwrap().created_worker.clone().unwrap()
    }

    fn captured_job_request(&self) -> service::JobRequest {
        self.0.lock().unwrap().enqueue_requests[0].clone()
    }

    fn captured_result_query(&self) -> FindJobResultListRequest {
        self.0.lock().unwrap().result_lookup[0].clone()
    }

    fn runner(&self) -> Runner {
        let runner = self.0.lock().unwrap().runner.clone();
        Runner {
            id: Some(RunnerId {
                value: RPC_RUNNER_ID,
            }),
            data: Some(runner),
        }
    }

    fn chat_result(&self, bytes: &[u8], done: bool) -> Vec<u8> {
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let model = self.0.lock().unwrap().model.clone();
        LlmChatResult {
            content: Some(MessageContent {
                content: Some(Content::Text(text)),
            }),
            usage: Some(
                grpc::proto::jobworkerp::runner::llm::llm_chat_result::Usage {
                    model,
                    ..Default::default()
                },
            ),
            done,
            ..Default::default()
        }
        .encode_to_vec()
    }
}

fn result_output_item(item: Item) -> ResultOutputItem {
    ResultOutputItem { item: Some(item) }
}

#[tonic::async_trait]
impl RunnerService for JudgeRpcFixture {
    type FindListStream = RpcStream<Runner>;
    type FindListByStream = RpcStream<Runner>;

    async fn create(
        &self,
        _request: Request<CreateRunnerRequest>,
    ) -> Result<Response<CreateRunnerResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn delete(
        &self,
        _request: Request<RunnerId>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn find(
        &self,
        _request: Request<RunnerId>,
    ) -> Result<Response<OptionalRunnerResponse>, Status> {
        Ok(Response::new(OptionalRunnerResponse {
            data: Some(self.runner()),
        }))
    }

    async fn find_by_name(
        &self,
        request: Request<RunnerNameRequest>,
    ) -> Result<Response<OptionalRunnerResponse>, Status> {
        let runner = self.runner();
        let data = request.into_inner().name.eq("LLM").then_some(runner);
        Ok(Response::new(OptionalRunnerResponse { data }))
    }

    async fn find_list(
        &self,
        _request: Request<FindListRequest>,
    ) -> Result<Response<Self::FindListStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn count(
        &self,
        _request: Request<CountCondition>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn find_list_by(
        &self,
        _request: Request<FindRunnerListRequest>,
    ) -> Result<Response<Self::FindListByStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn count_by(
        &self,
        _request: Request<CountRunnerRequest>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }
}

#[allow(deprecated)]
#[tonic::async_trait]
impl WorkerService for JudgeRpcFixture {
    type FindListStream = RpcStream<Worker>;

    async fn create(
        &self,
        request: Request<WorkerData>,
    ) -> Result<Response<CreateWorkerResponse>, Status> {
        let data = request.into_inner();
        let mut calls = self.0.lock().unwrap();
        calls.created_worker = Some(data.clone());
        calls.worker = Some(Worker {
            id: Some(WorkerId {
                value: RPC_WORKER_ID,
            }),
            data: Some(data),
        });
        Ok(Response::new(CreateWorkerResponse {
            id: Some(WorkerId {
                value: RPC_WORKER_ID,
            }),
        }))
    }

    async fn upsert_by_name(
        &self,
        _request: Request<WorkerData>,
    ) -> Result<Response<CreateWorkerResponse>, Status> {
        Err(Status::unimplemented("the adapter must use Create"))
    }

    async fn update(&self, _request: Request<Worker>) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn delete(
        &self,
        _request: Request<WorkerId>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn find(
        &self,
        _request: Request<WorkerId>,
    ) -> Result<Response<OptionalWorkerResponse>, Status> {
        Ok(Response::new(OptionalWorkerResponse {
            data: self.0.lock().unwrap().worker.clone(),
        }))
    }

    async fn find_by_name(
        &self,
        request: Request<WorkerNameRequest>,
    ) -> Result<Response<OptionalWorkerResponse>, Status> {
        let name = request.into_inner().name;
        let delay = self.0.lock().unwrap().worker_lookup_delay;
        tokio::time::sleep(delay).await;
        let mut calls = self.0.lock().unwrap();
        calls.worker_lookups += 1;
        let lookup_number = calls.worker_lookups;
        if calls.mutate_worker_on_lookup == Some(lookup_number)
            && let Some(worker) = calls.worker.as_mut()
            && let Some(data) = worker.data.as_mut()
        {
            data.description.push_str(" changed before dispatch");
        }
        let data = calls
            .worker
            .as_ref()
            .filter(|worker| worker.data.as_ref().is_some_and(|data| data.name == name))
            .cloned();
        Ok(Response::new(OptionalWorkerResponse { data }))
    }

    async fn find_list(
        &self,
        _request: Request<FindWorkerListRequest>,
    ) -> Result<Response<Self::FindListStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn count(
        &self,
        _request: Request<CountCondition>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn count_by(
        &self,
        _request: Request<CountWorkerRequest>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn find_channel_list(
        &self,
        _request: Request<jobworkerp_client::jobworkerp::data::Empty>,
    ) -> Result<Response<service::FindChannelListResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn release_static_worker(
        &self,
        _request: Request<ReleaseStaticWorkerRequest>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn load(
        &self,
        _request: Request<service::LoadWorkerRequest>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }
}

#[allow(deprecated)]
#[tonic::async_trait]
impl JobService for JudgeRpcFixture {
    type EnqueueForStreamStream = RpcStream<ResultOutputItem>;
    type EnqueueForResultStream = RpcStream<JobResult>;
    type EnqueueWithClientStreamStream = RpcStream<ResultOutputItem>;
    type FindListStream = RpcStream<Job>;
    type FindQueueListStream = RpcStream<JobAndStatus>;
    type FindListWithProcessingStatusStream = RpcStream<JobAndStatus>;

    async fn enqueue(
        &self,
        request: Request<service::JobRequest>,
    ) -> Result<Response<CreateJobResponse>, Status> {
        let response = self.enqueue_for_stream(request).await?;
        drop(response);
        let mut calls = self.0.lock().unwrap();
        let delivery = calls.delivery;
        let data = calls.job_result.as_mut().unwrap().data.as_mut().unwrap();
        data.response_type = ResponseType::NoResult as i32;
        if delivery != RpcDelivery::Complete {
            data.status = ResultStatus::OtherError as i32;
        }
        Ok(Response::new(CreateJobResponse {
            id: Some(JobId { value: RPC_JOB_ID }),
            result: None,
        }))
    }

    async fn enqueue_for_result(
        &self,
        _request: Request<service::JobRequest>,
    ) -> Result<Response<Self::EnqueueForResultStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn enqueue_for_stream(
        &self,
        request: Request<service::JobRequest>,
    ) -> Result<Response<Self::EnqueueForStreamStream>, Status> {
        let request = request.into_inner();
        let (response, delivery, worker, result_job_id) = {
            let mut calls = self.0.lock().unwrap();
            calls.enqueue_requests.push(request.clone());
            let response = calls.response.clone();
            let delivery = calls.delivery;
            let worker = calls.worker.clone().unwrap();
            (response, delivery, worker, calls.result_job_id)
        };
        let full_result = self.chat_result(&response, true);
        let worker_id = worker.id.expect("fixture worker id");
        let job_id = JobId { value: RPC_JOB_ID };
        let job_args = request.args.clone();
        let uniq_key = request.uniq_key.clone();
        let job_result = JobResult {
            id: Some(JobResultId { value: RPC_JOB_ID }),
            data: Some(JobResultData {
                job_id: Some(JobId {
                    value: result_job_id,
                }),
                worker_id: Some(worker_id),
                worker_name: worker.data.unwrap().name,
                args: job_args,
                uniq_key,
                status: ResultStatus::Success as i32,
                output: Some(ResultOutput {
                    items: full_result.clone(),
                }),
                response_type: ResponseType::Direct as i32,
                store_success: true,
                store_failure: true,
                using: Some("chat".to_owned()),
                broadcast_results: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        self.0.lock().unwrap().job_result = Some(job_result);

        let items = match delivery {
            RpcDelivery::Complete => vec![
                Ok(result_output_item(Item::Data(full_result))),
                Ok(result_output_item(Item::End(Trailer::default()))),
            ],
            RpcDelivery::CompleteWithErrorTrailer => vec![
                Ok(result_output_item(Item::Data(full_result))),
                Ok(result_output_item(Item::End(Trailer { metadata: std::collections::HashMap::from([("stream_error".into(), r#"{"version":1,"code":"PROVIDER_STREAM_FAILED","message":"interrupted","origin":"LLM"}"#.into())]), ..Default::default() }))),
            ],
            RpcDelivery::CompleteWithTypedErrorTrailer => vec![
                Ok(result_output_item(Item::Data(full_result))),
                Ok(result_output_item(Item::End(Trailer {
                    error: Some(jobworkerp_client::jobworkerp::data::StreamError {
                        code: jobworkerp_client::jobworkerp::data::StreamErrorCode::ProviderStreamFailed
                            as i32,
                        message: "interrupted".to_owned(),
                        origin: "LLM".to_owned(),
                        problem: None,
                    }),
                    ..Default::default()
                }))),
            ],
            RpcDelivery::CollectedWithoutEnd => {
                vec![Ok(result_output_item(Item::FinalCollected(full_result)))]
            }
            RpcDelivery::LoseStreamAfterEnqueue => vec![
                Ok(result_output_item(Item::Data(
                    self.chat_result(b"partial", false),
                ))),
                Err(Status::unavailable(
                    "fixture dropped stream after accepting job",
                )),
            ],
        };
        let mut response =
            Response::new(Box::pin(stream::iter(items)) as Self::EnqueueForStreamStream);
        let encoded_job_id = job_id.encode_to_vec();
        response.metadata_mut().insert_bin(
            JOB_ID_HEADER_NAME,
            MetadataValue::from_bytes(&encoded_job_id),
        );
        let gate = self.0.lock().unwrap().enqueue_response_gate.clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        Ok(response)
    }

    async fn enqueue_with_client_stream(
        &self,
        _request: Request<tonic::Streaming<ClientStreamRequest>>,
    ) -> Result<Response<Self::EnqueueWithClientStreamStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn delete(&self, request: Request<JobId>) -> Result<Response<SuccessResponse>, Status> {
        self.0
            .lock()
            .unwrap()
            .cancelled_jobs
            .push(request.into_inner().value);
        Ok(Response::new(SuccessResponse { is_success: true }))
    }

    async fn find(
        &self,
        _request: Request<JobId>,
    ) -> Result<Response<OptionalJobResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn find_list(
        &self,
        _request: Request<FindListRequest>,
    ) -> Result<Response<Self::FindListStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn find_queue_list(
        &self,
        _request: Request<FindQueueListRequest>,
    ) -> Result<Response<Self::FindQueueListStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn find_list_with_processing_status(
        &self,
        _request: Request<FindListWithProcessingStatusRequest>,
    ) -> Result<Response<Self::FindListWithProcessingStatusStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn count(
        &self,
        _request: Request<CountCondition>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }
}

#[allow(deprecated)]
#[tonic::async_trait]
impl JobResultService for JudgeRpcFixture {
    type FindListStream = RpcStream<JobResult>;
    type FindListByJobIdStream = RpcStream<JobResult>;
    type ListenStreamStream = RpcStream<ResultOutputItem>;
    type ListenByWorkerStream = RpcStream<JobResult>;
    type FindListByStream = RpcStream<JobResult>;

    async fn delete(
        &self,
        _request: Request<JobResultId>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn find(
        &self,
        _request: Request<JobResultId>,
    ) -> Result<Response<OptionalJobResultResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn find_list(
        &self,
        _request: Request<FindListRequest>,
    ) -> Result<Response<Self::FindListStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn find_list_by_job_id(
        &self,
        _request: Request<FindListByJobIdRequest>,
    ) -> Result<Response<Self::FindListByJobIdStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn listen(
        &self,
        _request: Request<ListenRequest>,
    ) -> Result<Response<JobResult>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn listen_stream(
        &self,
        _request: Request<ListenRequest>,
    ) -> Result<Response<Self::ListenStreamStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn listen_by_worker(
        &self,
        _request: Request<ListenByWorkerRequest>,
    ) -> Result<Response<Self::ListenByWorkerStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn count(
        &self,
        _request: Request<CountCondition>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn find_list_by(
        &self,
        request: Request<FindJobResultListRequest>,
    ) -> Result<Response<Self::FindListByStream>, Status> {
        let mut calls = self.0.lock().unwrap();
        calls.result_lookup.push(request.into_inner());
        if calls.result_lookup_error {
            return Err(Status::unavailable("result store unavailable"));
        }
        let mut result = calls.job_result.clone();
        if calls.empty_result_lookups > 0 {
            calls.empty_result_lookups -= 1;
            result = None;
        }
        if let (Some(result), Some(mutation)) = (result.as_mut(), calls.result_mutation) {
            mutation(result);
        }
        let mut results = result.into_iter().map(Ok).collect::<Vec<_>>();
        if calls.duplicate_result
            && let Some(Ok(result)) = results.first()
        {
            results.push(Ok(result.clone()));
        }
        Ok(Response::new(
            Box::pin(stream::iter(results)) as Self::FindListByStream
        ))
    }

    async fn count_by(
        &self,
        _request: Request<CountJobResultRequest>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn delete_bulk(
        &self,
        _request: Request<DeleteJobResultBulkRequest>,
    ) -> Result<Response<DeleteJobResultBulkResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }
}

struct MockJobworkerpServer {
    endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
}

impl Drop for MockJobworkerpServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.join.abort();
    }
}

async fn start_jobworkerp_server(fixture: JudgeRpcFixture) -> MockJobworkerpServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (incoming_tx, incoming_rx) = mpsc::channel::<std::io::Result<TcpStream>>(4);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            if incoming_tx.send(Ok(stream)).await.is_err() {
                break;
            }
        }
    });
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let join = tokio::spawn(async move {
        Server::builder()
            .add_service(RunnerServiceServer::new(fixture.clone()))
            .add_service(WorkerServiceServer::new(fixture.clone()))
            .add_service(JobServiceServer::new(fixture.clone()))
            .add_service(JobResultServiceServer::new(fixture))
            .serve_with_incoming_shutdown(ReceiverStream::new(incoming_rx), async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    MockJobworkerpServer {
        endpoint: format!("http://{address}"),
        shutdown: Some(shutdown_tx),
        join,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_rpc_bridge_registers_exact_worker_and_sends_only_approved_utf8_messages() {
    use grpc::proto::jobworkerp::runner::llm::{
        LlmChatArgs, llm_chat_args::ChatRole,
        llm_chat_args::message_content::Content as ArgsContent,
    };

    let directory = tempfile::tempdir().unwrap();
    let model = "gpt-4o-2024-08-06";
    let base_url = "https://llm.example.test/v1";
    let settings = external_settings(model, Some(base_url));
    let selection = judge_provider::provider_selection_for_settings(&settings).unwrap();
    let expected_worker_name = format!("lookback-coding-agent-judge-{}", selection.config_version);
    let deadline = current_unix_ms() + 20_000;
    let prepared = prepared_assessment("rpc-complete", deadline);
    let fixture = JudgeRpcFixture::new(
        valid_assessment_response(&prepared),
        model,
        RpcDelivery::Complete,
    );
    let server = start_jobworkerp_server(fixture.clone()).await;

    let outcome = judge_provider::assess_with_jobworkerp(
        &server.endpoint,
        judge_provider::ApprovedJudgeProviderProfile::from_settings(
            &settings,
            selection.clone(),
            deadline,
        )
        .unwrap(),
        AssessmentEngine::new(AssessmentStore::new(directory.path()).unwrap()),
        prepared.clone(),
        FixedClock(current_unix_ms()),
    )
    .await
    .unwrap();

    assert!(matches!(outcome, AssessmentOutcome::Assessed { .. }));
    assert_eq!(fixture.enqueue_count(), 1);
    assert_eq!(fixture.result_lookup_count(), 0);

    let worker = fixture.captured_worker();
    assert_eq!(worker.runner_id.as_ref().unwrap().value, RPC_RUNNER_ID);
    assert_eq!(worker.name, expected_worker_name);
    assert_eq!(worker.channel.as_deref(), Some("llm_external"));
    assert_eq!(worker.queue_type, QueueType::Normal as i32);
    assert_eq!(worker.response_type, ResponseType::Direct as i32);
    assert!(worker.store_success && worker.store_failure && worker.broadcast_results);
    assert!(!worker.use_static);
    assert_eq!(worker.periodic_interval, 0);
    assert_eq!(
        worker.retry_policy.as_ref().unwrap().r#type,
        jobworkerp_client::jobworkerp::data::RetryType::None as i32
    );
    let runner_data = fixture.runner().data.unwrap();
    let settings_descriptor =
        JobworkerpProto::parse_runner_settings_schema_descriptor(&runner_data)
            .unwrap()
            .unwrap();
    let expected_settings = JobworkerpProto::json_value_to_message(
        settings_descriptor,
        &serde_json::json!({ "genai": { "model": model, "base_url": base_url } }),
        true,
        false,
    )
    .unwrap();
    assert_eq!(worker.runner_settings, expected_settings);

    let job = fixture.captured_job_request();
    assert!(matches!(
        job.worker,
        Some(service::job_request::Worker::WorkerId(WorkerId {
            value: RPC_WORKER_ID
        }))
    ));
    assert_eq!(job.using.as_deref(), Some("chat"));
    let unique_key = format!(
        "coding-agent-assessment-v1:{}:{}",
        prepared.assessment_id(),
        prepared.judge_input_digest()
    );
    assert_eq!(job.uniq_key.as_deref(), Some(unique_key.as_str()));
    let overrides = job.overrides.unwrap();
    assert_eq!(overrides.response_type, Some(ResponseType::Direct as i32));
    assert_eq!(overrides.store_success, Some(true));
    assert_eq!(overrides.store_failure, Some(true));
    assert_eq!(overrides.broadcast_results, Some(true));
    assert_eq!(
        overrides.retry_policy.unwrap().r#type,
        jobworkerp_client::jobworkerp::data::RetryType::None as i32
    );

    let args = LlmChatArgs::decode(job.args.as_slice()).unwrap();
    assert_eq!(args.messages.len(), 2);
    assert_eq!(args.messages[0].role, ChatRole::System as i32);
    assert_eq!(args.messages[1].role, ChatRole::User as i32);
    let text_at = |index: usize| match args.messages[index]
        .content
        .as_ref()
        .and_then(|content| content.content.as_ref())
    {
        Some(ArgsContent::Text(text)) => text.as_str(),
        other => panic!("expected exact text chat content, got {other:?}"),
    };
    assert_eq!(
        text_at(0),
        std::str::from_utf8(prepared.system_policy_bytes()).unwrap()
    );
    let user_message = text_at(1);
    assert_eq!(user_message.as_bytes(), prepared.input_bytes());
    assert!(user_message.contains("café"));
    assert!(user_message.contains("—"));
    assert!(args.model.is_none());
    assert!(args.options.is_none());
    assert!(args.function_options.is_none());
    assert!(args.json_schema.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_stream_recovers_exact_job_result_without_reenqueuing_same_unique_key() {
    let directory = tempfile::tempdir().unwrap();
    let model = "gpt-4o-2024-08-06";
    let deadline = current_unix_ms() + 20_000;
    let prepared = prepared_assessment("rpc-lost-stream", deadline);
    let fixture = JudgeRpcFixture::new(
        valid_assessment_response(&prepared),
        model,
        RpcDelivery::LoseStreamAfterEnqueue,
    );
    let server = start_jobworkerp_server(fixture.clone()).await;

    let outcome = judge_provider::assess_with_jobworkerp(
        &server.endpoint,
        provider_profile(deadline),
        AssessmentEngine::new(AssessmentStore::new(directory.path()).unwrap()),
        prepared.clone(),
        FixedClock(current_unix_ms()),
    )
    .await
    .unwrap();

    assert!(matches!(outcome, AssessmentOutcome::Assessed { .. }));
    assert_eq!(fixture.enqueue_count(), 1);
    assert_eq!(fixture.result_lookup_count(), 1);
    let job = fixture.captured_job_request();
    let query = fixture.captured_result_query();
    assert_eq!(
        query.worker_ids,
        vec![WorkerId {
            value: RPC_WORKER_ID
        }]
    );
    assert_eq!(query.uniq_key, job.uniq_key);
    assert_eq!(query.limit, Some(2));
    let result = fixture.0.lock().unwrap().job_result.clone().unwrap();
    let result_data = result.data.unwrap();
    assert_eq!(result_data.job_id.unwrap().value, RPC_JOB_ID);
    assert_eq!(result_data.args, job.args);
    assert_eq!(result_data.uniq_key, job.uniq_key);
    assert!(!judge_provider::same_key_idempotent_replay_is_supported());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_snapshot_worker_is_rejected_before_enqueue_and_cannot_pass() {
    let directory = tempfile::tempdir().unwrap();
    let deadline = current_unix_ms() + 20_000;
    let prepared = prepared_assessment("rpc-worker-mutated", deadline);
    let fixture = JudgeRpcFixture::new(
        valid_assessment_response(&prepared),
        "gpt-4o-2024-08-06",
        RpcDelivery::Complete,
    );
    fixture.set_worker_mutation(3);
    let server = start_jobworkerp_server(fixture.clone()).await;

    let outcome = judge_provider::assess_with_jobworkerp(
        &server.endpoint,
        provider_profile(deadline),
        AssessmentEngine::new(AssessmentStore::new(directory.path()).unwrap()),
        prepared,
        FixedClock(current_unix_ms()),
    )
    .await
    .unwrap();

    assert!(matches!(outcome, AssessmentOutcome::Pending { .. }));
    assert_eq!(fixture.enqueue_count(), 0);
    assert_eq!(fixture.result_lookup_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipt_result_must_match_the_job_id_from_enqueue_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let deadline = current_unix_ms() + 20_000;
    let prepared = prepared_assessment("rpc-job-id-mismatch", deadline);
    let fixture = JudgeRpcFixture::new(
        valid_assessment_response(&prepared),
        "gpt-4o-2024-08-06",
        RpcDelivery::LoseStreamAfterEnqueue,
    );
    fixture.set_result_job_id(RPC_JOB_ID + 1);
    let server = start_jobworkerp_server(fixture.clone()).await;

    let outcome = judge_provider::assess_with_jobworkerp(
        &server.endpoint,
        provider_profile(deadline),
        AssessmentEngine::new(AssessmentStore::new(directory.path()).unwrap()),
        prepared,
        FixedClock(current_unix_ms()),
    )
    .await
    .unwrap();

    assert!(matches!(outcome, AssessmentOutcome::Pending { .. }));
    assert_eq!(fixture.enqueue_count(), 1);
    assert_eq!(fixture.result_lookup_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_resolved_model_names_do_not_replace_the_bound_request_configuration() {
    for delivery in [RpcDelivery::Complete, RpcDelivery::LoseStreamAfterEnqueue] {
        let directory = tempfile::tempdir().unwrap();
        let deadline = current_unix_ms() + 20_000;
        let prepared = prepared_assessment("resolved-model", deadline);
        let fixture = JudgeRpcFixture::new(
            valid_assessment_response(&prepared),
            "provider/resolved-model",
            delivery,
        );
        let server = start_jobworkerp_server(fixture.clone()).await;
        let outcome = judge_provider::assess_with_jobworkerp(
            &server.endpoint,
            provider_profile(deadline),
            AssessmentEngine::new(AssessmentStore::new(directory.path()).unwrap()),
            prepared,
            FixedClock(current_unix_ms()),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, AssessmentOutcome::Assessed { .. }));
        assert_eq!(fixture.enqueue_count(), 1);
    }
}

#[test]
fn selection_is_derived_from_the_configured_provider_model_and_nonsecret_config() {
    let selection = judge_provider::provider_selection_for_settings(&external_settings(
        "gpt-4o-2024-08-06",
        Some("https://llm.example.test/v1"),
    ))
    .unwrap();

    assert_eq!(selection.provider_id, "openai");
    assert_eq!(selection.model_id, "gpt-4o-2024-08-06");
    assert_eq!(selection.model_version, "gpt-4o-2024-08-06");
    assert_eq!(selection.config_version.len(), 64);

    let same = judge_provider::provider_selection_for_settings(&external_settings(
        "gpt-4o-2024-08-06",
        Some("https://llm.example.test/v1"),
    ))
    .unwrap();
    let different_endpoint = judge_provider::provider_selection_for_settings(&external_settings(
        "gpt-4o-2024-08-06",
        Some("https://other.example.test/v1"),
    ))
    .unwrap();

    assert_eq!(selection, same);
    assert_ne!(selection.config_version, different_endpoint.config_version);
}

#[test]
fn aliases_and_other_configured_providers_do_not_require_a_dated_model_name() {
    for (model, endpoint, expected_provider) in [
        ("gpt-4o", None, "openai"),
        ("claude-sonnet-4", None, "anthropic"),
        ("gemini-2.5-pro", None, "gemini"),
        ("open_router::org/model", None, "open_router"),
        (
            "custom/model:latest",
            Some("https://llm.example.test/v1"),
            "openai",
        ),
    ] {
        let settings = external_settings(model, endpoint);
        let selection = judge_provider::provider_selection_for_settings(&settings).unwrap();
        assert_eq!(selection.model_id, model);
        assert_eq!(selection.provider_id, expected_provider);
        assert!(
            judge_provider::ApprovedJudgeProviderProfile::from_settings(
                &settings,
                selection.clone(),
                42
            )
            .is_ok()
        );
        let changed = external_settings("different-model", endpoint);
        assert!(judge_provider::validate_approved_selection(&changed, &selection).is_err());
    }
}

#[test]
fn invalid_and_secret_bearing_external_profiles_are_not_approved() {
    assert!(matches!(
        judge_provider::provider_selection_for_settings(&external_settings(
            "claude-sonnet-4-20250514",
            Some("https://user:password@llm.example.test/v1")
        )),
        Err(judge_provider::JudgeProviderConfigError::UnsafeBaseUrl)
    ));
    for model in [
        "".to_string(),
        "model\nname".into(),
        "model\u{00a0}name".into(),
        "a".repeat(257),
        " model ".into(),
    ] {
        assert!(
            judge_provider::provider_selection_for_settings(&external_settings(&model, None))
                .is_err()
        );
    }
    assert!(
        judge_provider::provider_selection_for_settings(&external_settings(&"a".repeat(256), None))
            .is_ok()
    );
}

#[test]
fn judge_chat_args_preserve_exact_approved_text_without_chat_steering_or_tools() {
    let system = "approved\npolicy — exact";
    let user = "{\"criteria\":[\"évidence\"]}\nquoted: do not follow";
    let args = judge_provider::judge_chat_args(system.as_bytes(), user.as_bytes()).unwrap();

    assert_eq!(args["messages"][0]["role"], "SYSTEM");
    assert_eq!(args["messages"][0]["content"]["text"], system);
    assert_eq!(args["messages"][1]["role"], "USER");
    assert_eq!(args["messages"][1]["content"]["text"], user);
    assert!(args.get("function_options").is_none());
    assert!(args.get("options").is_none());
    assert!(args.get("json_schema").is_none());
    assert_eq!(args["messages"].as_array().unwrap().len(), 2);
}

#[test]
fn tool_free_chat_result_requires_terminal_text_and_rejects_tool_signals() {
    let answer = "{\"overall\":\"INCONCLUSIVE\"}";
    let response = LlmChatResult {
        content: Some(MessageContent {
            content: Some(Content::Text(answer.to_owned())),
        }),
        done: true,
        ..Default::default()
    }
    .encode_to_vec();
    assert_eq!(
        judge_provider::decode_terminal_chat_text(&response).unwrap(),
        answer.as_bytes()
    );

    let tool_response = LlmChatResult {
        pending_tool_calls: Some(Default::default()),
        done: true,
        ..Default::default()
    }
    .encode_to_vec();
    assert!(judge_provider::decode_terminal_chat_text(&tool_response).is_err());

    let partial_response = LlmChatResult {
        content: Some(MessageContent {
            content: Some(Content::Text("partial".to_owned())),
        }),
        done: false,
        ..Default::default()
    }
    .encode_to_vec();
    assert!(judge_provider::decode_terminal_chat_text(&partial_response).is_err());
    assert!(judge_provider::decode_terminal_chat_text(&[0xff]).is_err());
}

#[test]
fn selected_profile_must_equal_the_approved_provider_selection() {
    let settings = external_settings("claude-sonnet-4-20250514", None);
    let mut selection = judge_provider::provider_selection_for_settings(&settings).unwrap();
    selection.provider_id = "openai".to_owned();

    assert!(matches!(
        judge_provider::validate_approved_selection(&settings, &selection),
        Err(judge_provider::JudgeProviderConfigError::SelectionMismatch)
    ));
}

#[test]
fn runtime_profile_uses_the_approved_worker_snapshot_name() {
    let settings = external_settings("claude-sonnet-4-20250514", None);
    let selection = judge_provider::provider_selection_for_settings(&settings).unwrap();
    let profile = judge_provider::ApprovedJudgeProviderProfile::from_settings(
        &settings,
        selection.clone(),
        u64::MAX,
    )
    .unwrap();

    assert_eq!(profile.selection(), &selection);
    assert_eq!(
        profile.worker_name(),
        format!("lookback-coding-agent-judge-{}", selection.config_version)
    );
}

#[test]
fn same_jobworkerp_unique_key_is_never_treated_as_durable_idempotent_replay() {
    assert!(!judge_provider::same_key_idempotent_replay_is_supported());
}

#[test]
fn receipt_binding_rejects_a_different_worker_or_serialized_prompt() {
    let binding = judge_provider::ReceiptBinding {
        worker_id: 71,
        worker_name: "coding-agent-judge-test".to_owned(),
        uniq_key: "coding-agent-assessment-v1:a:d".to_owned(),
        expected_args: vec![1, 2, 3],
        job_id: Some(19),
    };

    assert!(binding.matches_result_envelope(
        71,
        "coding-agent-judge-test",
        "coding-agent-assessment-v1:a:d",
        &[1, 2, 3],
        19,
    ));
    assert!(!binding.matches_result_envelope(
        72,
        "coding-agent-judge-test",
        "coding-agent-assessment-v1:a:d",
        &[1, 2, 3],
        19,
    ));
    assert!(!binding.matches_result_envelope(
        71,
        "coding-agent-judge-test",
        "coding-agent-assessment-v1:a:d",
        &[1, 2, 4],
        19,
    ));
    assert!(!binding.matches_result_envelope(
        71,
        "coding-agent-judge-test",
        "coding-agent-assessment-v1:a:d",
        &[1, 2, 3],
        20,
    ));
}

#[test]
fn the_external_adapter_rejects_local_settings_without_rejecting_external_aliases() {
    let local = LlmSettings {
        mode: LlmMode::Local,
        ..LlmSettings::default()
    };
    assert!(matches!(
        judge_provider::provider_selection_for_settings(&local),
        Err(judge_provider::JudgeProviderConfigError::UnsupportedProvider)
    ));

    let unversioned_anthropic = external_settings("claude-sonnet-4", None);
    assert!(judge_provider::provider_selection_for_settings(&unversioned_anthropic).is_ok());
}

#[tokio::test]
async fn saved_final_resumes_after_deadline_without_connecting_to_offline_jobworkerp() {
    let directory = tempfile::tempdir().unwrap();
    let deadline = current_unix_ms() + 250;
    let prepared = prepared_assessment("saved-final", deadline);
    let expected = AssessmentEngine::new(AssessmentStore::new(directory.path()).unwrap())
        .assess(
            &prepared,
            &mut SeedProvider {
                response: valid_assessment_response(&prepared),
                lookup: ReceiptLookup::Unsupported,
            },
            &FixedClock(deadline - 1),
        )
        .unwrap();
    assert!(matches!(expected, AssessmentOutcome::Assessed { .. }));
    wait_until(deadline).await;

    let resumed = judge_provider::resume_with_jobworkerp(
        &offline_endpoint().await,
        provider_profile(deadline),
        AssessmentEngine::new(AssessmentStore::new(directory.path()).unwrap()),
        prepared,
        FixedClock(deadline + 1),
    )
    .await
    .expect("durable final outcome must not need jobworkerp availability");

    assert_eq!(resumed, expected);
}

#[tokio::test]
async fn timely_saved_response_resumes_after_deadline_without_network_or_replay() {
    let directory = tempfile::tempdir().unwrap();
    let deadline = current_unix_ms() + 250;
    let prepared = prepared_assessment("saved-response", deadline);
    let first = AssessmentEngine::new(AssessmentStore::new(directory.path()).unwrap())
        .assess(
            &prepared,
            &mut SeedProvider {
                response: valid_assessment_response(&prepared),
                lookup: ReceiptLookup::Unsupported,
            },
            &FixedClock(deadline - 1),
        )
        .unwrap();
    assert!(matches!(first, AssessmentOutcome::Assessed { .. }));
    std::fs::remove_file(directory.path().join("saved-response/final.json")).unwrap();
    wait_until(deadline).await;

    let resumed = judge_provider::resume_with_jobworkerp(
        &offline_endpoint().await,
        provider_profile(deadline),
        AssessmentEngine::new(AssessmentStore::new(directory.path()).unwrap()),
        prepared,
        FixedClock(deadline + 1),
    )
    .await
    .expect("timely response observation must be finalized from disk after restart");

    assert!(matches!(resumed, AssessmentOutcome::Assessed { .. }));
}

#[tokio::test]
async fn expired_pending_assessment_becomes_inconclusive_without_connecting_or_replaying() {
    let directory = tempfile::tempdir().unwrap();
    let deadline = current_unix_ms() + 2_000;
    let prepared = prepared_assessment("expired-pending", deadline);
    let fixture = JudgeRpcFixture::new(
        valid_assessment_response(&prepared),
        "gpt-4o-2024-08-06",
        RpcDelivery::LoseStreamAfterEnqueue,
    );
    fixture.set_result_lookup_error();
    let server = start_jobworkerp_server(fixture.clone()).await;
    let pending = judge_provider::assess_with_jobworkerp(
        &server.endpoint,
        provider_profile(deadline),
        AssessmentEngine::new(AssessmentStore::new(directory.path()).unwrap()),
        prepared.clone(),
        FixedClock(deadline - 1),
    )
    .await
    .unwrap();
    assert!(matches!(pending, AssessmentOutcome::Pending { .. }));
    assert_eq!(fixture.enqueue_count(), 1);
    assert_eq!(fixture.result_lookup_count(), 1);
    drop(server);
    wait_until(deadline).await;

    let expired = judge_provider::resume_with_jobworkerp(
        &offline_endpoint().await,
        provider_profile(deadline),
        AssessmentEngine::new(AssessmentStore::new(directory.path()).unwrap()),
        prepared,
        FixedClock(deadline + 1),
    )
    .await
    .expect("expired unresolved dispatch must finalize locally when jobworkerp is offline");

    assert!(matches!(expired, AssessmentOutcome::Inconclusive(_)));
}

#[allow(dead_code)]
fn _type_contracts_remain_owned_by_the_library() {
    let _: Option<ProviderSelection> = None;
}

fn ordinary_judge_fixture(
    mode: LlmMode,
    response: &[u8],
    delivery: RpcDelivery,
) -> JudgeRpcFixture {
    let fixture = JudgeRpcFixture::new(response.to_vec(), "resolved-model", delivery);
    {
        let mut calls = fixture.0.lock().unwrap();
        if mode == LlmMode::External {
            calls
                .runner
                .method_proto_map
                .as_mut()
                .unwrap()
                .schemas
                .get_mut("chat")
                .unwrap()
                .output_type = StreamingOutputType::Both as i32;
        }
        calls.runner.name = if mode == LlmMode::Local {
            "LLMPromptRunner"
        } else {
            "LLM"
        }
        .into();
        calls.worker = Some(Worker {
            id: Some(WorkerId { value: 42 }),
            data: Some(WorkerData {
                name: commands::llm_settings::worker_name_for(mode).into(),
                runner_id: Some(RunnerId {
                    value: RPC_RUNNER_ID,
                }),
                runner_settings: b"actual configured model settings".to_vec(),
                use_static: mode == LlmMode::Local,
                response_type: ResponseType::Direct as i32,
                ..Default::default()
            }),
        });
    }
    fixture
}

#[tokio::test]
async fn ordinary_judge_uses_the_existing_local_or_external_worker_without_registering_another_model()
 {
    use coding_agent::semantic_judge::RegisteredSemanticJudge;
    for mode in [LlmMode::Local, LlmMode::External] {
        let fixture = ordinary_judge_fixture(mode, br#"{"outcome":"PASS"}"#, RpcDelivery::Complete);
        let server = start_jobworkerp_server(fixture.clone()).await;
        let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
            .await
            .unwrap();
        let judge = RegisteredSemanticJudge::observe(&handle, mode)
            .await
            .unwrap();
        let bytes = judge
            .evaluate(
                &handle,
                b"independent judge",
                b"mounted diff",
                "ordinary-run:attempt:judge",
            )
            .await
            .unwrap();
        assert_eq!(bytes, br#"{"outcome":"PASS"}"#);
        let request = fixture.captured_job_request();
        assert_eq!(
            request.worker,
            Some(service::job_request::Worker::WorkerId(WorkerId {
                value: 42
            }))
        );
        assert_eq!(request.using.as_deref(), Some("chat"));
        assert_eq!(
            request.uniq_key.as_deref(),
            Some("ordinary-run:attempt:judge")
        );
        let args =
            grpc::proto::jobworkerp::runner::llm::LlmChatArgs::decode(request.args.as_slice())
                .unwrap();
        assert_eq!(args.messages.len(), 2);
        assert!(!args.function_options.unwrap().use_function_calling);
        assert!(fixture.0.lock().unwrap().created_worker.is_none());
        drop(server);
    }
}

#[tokio::test]
async fn ordinary_external_judge_collects_the_final_job_result_without_response_streaming() {
    use coding_agent::semantic_judge::{RegisteredSemanticJudge, SemanticJudgePolicy};
    let fixture = ordinary_judge_fixture(
        LlmMode::External,
        br#"{"outcome":"PASS"}"#,
        RpcDelivery::Complete,
    );
    let server = start_jobworkerp_server(fixture.clone()).await;
    let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
        .await
        .unwrap();
    let judge = RegisteredSemanticJudge::observe(&handle, LlmMode::External)
        .await
        .unwrap()
        .with_policy(SemanticJudgePolicy {
            max_retries: 2,
            deadline_seconds: 125,
        })
        .unwrap();
    let output = judge
        .evaluate(&handle, b"judge", b"source", "external:final-result")
        .await
        .unwrap();
    assert_eq!(output, br#"{"outcome":"PASS"}"#);
    let overrides = fixture.captured_job_request().overrides.unwrap();
    assert_eq!(overrides.response_type, Some(ResponseType::NoResult as i32));
    assert_eq!(overrides.store_success, Some(true));
    assert_eq!(overrides.store_failure, Some(true));
    assert_eq!(overrides.retry_policy.unwrap().max_retry, 2);
    assert_eq!(fixture.enqueue_count(), 1);
    assert!(!fixture.0.lock().unwrap().result_lookup.is_empty());
}

#[tokio::test]
async fn ordinary_external_judge_rejects_failed_or_mismatched_final_results_without_redispatch() {
    use coding_agent::semantic_judge::RegisteredSemanticJudge;
    for (delivery, wrong_job, unavailable_lookup) in [
        (RpcDelivery::LoseStreamAfterEnqueue, false, false),
        (RpcDelivery::Complete, true, false),
        (RpcDelivery::Complete, false, true),
    ] {
        let fixture = ordinary_judge_fixture(LlmMode::External, br#"{"outcome":"PASS"}"#, delivery);
        {
            let mut calls = fixture.0.lock().unwrap();
            if wrong_job {
                calls.result_job_id = RPC_JOB_ID + 1;
            }
            calls.result_lookup_error = unavailable_lookup;
        }
        let server = start_jobworkerp_server(fixture.clone()).await;
        let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
            .await
            .unwrap();
        let judge = RegisteredSemanticJudge::observe(&handle, LlmMode::External)
            .await
            .unwrap();
        assert!(
            judge
                .evaluate(&handle, b"judge", b"source", "external:final-result")
                .await
                .is_err()
        );
        assert_eq!(fixture.enqueue_count(), 1);
    }
}

#[tokio::test]
async fn ordinary_external_judge_waits_for_saved_results_without_resending_the_job() {
    use coding_agent::semantic_judge::RegisteredSemanticJudge;
    let fixture = ordinary_judge_fixture(
        LlmMode::External,
        br#"{"outcome":"PASS"}"#,
        RpcDelivery::Complete,
    );
    fixture.0.lock().unwrap().empty_result_lookups = 2;
    let server = start_jobworkerp_server(fixture.clone()).await;
    let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
        .await
        .unwrap();
    let judge = RegisteredSemanticJudge::observe(&handle, LlmMode::External)
        .await
        .unwrap();
    assert_eq!(
        judge
            .evaluate(&handle, b"judge", b"source", "external:poll")
            .await
            .unwrap(),
        br#"{"outcome":"PASS"}"#
    );
    assert_eq!(fixture.enqueue_count(), 1);
    assert_eq!(fixture.0.lock().unwrap().result_lookup.len(), 3);
}

#[tokio::test]
async fn ordinary_external_judge_accepts_bound_results_with_reconstructed_delivery_metadata() {
    use coding_agent::semantic_judge::{RegisteredSemanticJudge, SemanticJudgePolicy};
    let fixture = ordinary_judge_fixture(
        LlmMode::External,
        br#"{"outcome":"PASS"}"#,
        RpcDelivery::Complete,
    );
    fixture.0.lock().unwrap().result_mutation = Some(|result| {
        let data = result.data.as_mut().unwrap();
        data.response_type = ResponseType::Direct as i32;
        data.store_success = false;
        data.store_failure = false;
        data.max_retry = 0;
        data.retried = 1;
    });
    let server = start_jobworkerp_server(fixture.clone()).await;
    let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
        .await
        .unwrap();
    let judge = RegisteredSemanticJudge::observe(&handle, LlmMode::External)
        .await
        .unwrap()
        .with_policy(SemanticJudgePolicy {
            max_retries: 1,
            deadline_seconds: 30,
        })
        .unwrap();
    assert_eq!(
        judge
            .evaluate(
                &handle,
                b"judge",
                b"source",
                "external:reconstructed-metadata"
            )
            .await
            .unwrap(),
        br#"{"outcome":"PASS"}"#
    );
    assert_eq!(fixture.enqueue_count(), 1);
    let overrides = fixture.captured_job_request().overrides.unwrap();
    assert_eq!(overrides.response_type, Some(ResponseType::NoResult as i32));
    assert_eq!(overrides.retry_policy.unwrap().max_retry, 1);
}

#[tokio::test]
async fn ordinary_external_judge_verifies_result_binding_and_terminal_content() {
    use coding_agent::semantic_judge::RegisteredSemanticJudge;
    let cases: &[fn(&mut JobResult)] = &[
        |r| r.data.as_mut().unwrap().worker_id = Some(WorkerId { value: 999 }),
        |r| r.data.as_mut().unwrap().worker_name = "another-worker".into(),
        |r| r.data.as_mut().unwrap().args.clear(),
        |r| r.data.as_mut().unwrap().uniq_key = Some("another-request".into()),
        |r| r.data.as_mut().unwrap().using = Some("run".into()),
        |r| r.data.as_mut().unwrap().retried = 6,
        |r| r.data.as_mut().unwrap().output = None,
        |r| {
            r.data.as_mut().unwrap().output.as_mut().unwrap().items = LlmChatResult {
                done: false,
                ..Default::default()
            }
            .encode_to_vec()
        },
        |r| {
            r.data.as_mut().unwrap().output.as_mut().unwrap().items = LlmChatResult {
                done: true,
                pending_tool_calls: Some(Default::default()),
                ..Default::default()
            }
            .encode_to_vec()
        },
    ];
    for mutation in cases.iter().copied().map(Some).chain(std::iter::once(None)) {
        let fixture = ordinary_judge_fixture(
            LlmMode::External,
            br#"{"outcome":"PASS"}"#,
            RpcDelivery::Complete,
        );
        {
            let mut calls = fixture.0.lock().unwrap();
            calls.result_mutation = mutation;
            calls.duplicate_result = mutation.is_none();
        }
        let server = start_jobworkerp_server(fixture.clone()).await;
        let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
            .await
            .unwrap();
        let judge = RegisteredSemanticJudge::observe(&handle, LlmMode::External)
            .await
            .unwrap();
        assert!(
            judge
                .evaluate(&handle, b"judge", b"source", "external:bound-result")
                .await
                .is_err()
        );
        assert_eq!(fixture.enqueue_count(), 1);
    }
}

#[tokio::test]
async fn ordinary_judge_applies_bounded_job_overrides_without_replaying_semantic_failures() {
    use coding_agent::semantic_judge::{RegisteredSemanticJudge, SemanticJudgePolicy};
    for delivery in [RpcDelivery::Complete, RpcDelivery::LoseStreamAfterEnqueue] {
        let fixture = ordinary_judge_fixture(LlmMode::Local, br#"{"outcome":"FAIL"}"#, delivery);
        let server = start_jobworkerp_server(fixture.clone()).await;
        let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
            .await
            .unwrap();
        let judge = RegisteredSemanticJudge::observe(&handle, LlmMode::Local)
            .await
            .unwrap();
        let initial_digest = judge.configuration_digest();
        let judge = judge
            .with_policy(SemanticJudgePolicy {
                max_retries: 2,
                deadline_seconds: 180,
            })
            .unwrap();
        assert_ne!(judge.configuration_digest(), initial_digest);
        let result = judge
            .evaluate(&handle, b"judge", b"source", "bounded:judge")
            .await;
        if matches!(delivery, RpcDelivery::Complete) {
            assert_eq!(result.unwrap(), br#"{"outcome":"FAIL"}"#);
        } else {
            assert!(result.is_err());
        }
        let request = fixture.captured_job_request();
        let retry = request.overrides.unwrap().retry_policy.unwrap();
        assert_eq!(retry.max_retry, 2);
        assert_ne!(retry.r#type, 0);
        assert!(
            request
                .timeout
                .is_some_and(|timeout| timeout > 0 && timeout <= 180_000)
        );
        assert_eq!(fixture.enqueue_count(), 1);
        assert!(fixture.0.lock().unwrap().created_worker.is_none());
        drop(server);
    }
}

#[tokio::test]
async fn ordinary_judge_deadline_during_worker_revalidation_does_not_enqueue() {
    for mode in [LlmMode::Local, LlmMode::External] {
        use coding_agent::semantic_judge::{RegisteredSemanticJudge, SemanticJudgePolicy};
        let fixture = ordinary_judge_fixture(mode, b"{}", RpcDelivery::Complete);
        let server = start_jobworkerp_server(fixture.clone()).await;
        let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
            .await
            .unwrap();
        let judge = RegisteredSemanticJudge::observe(&handle, mode)
            .await
            .unwrap()
            .with_policy(SemanticJudgePolicy {
                max_retries: 2,
                deadline_seconds: 10,
            })
            .unwrap();
        fixture.0.lock().unwrap().worker_lookup_delay = Duration::from_secs(11);
        assert!(
            tokio::time::timeout(
                Duration::from_secs(12),
                judge.begin(&handle, b"judge", b"source", "lookup:deadline")
            )
            .await
            .unwrap()
            .is_err()
        );
        assert_eq!(fixture.enqueue_count(), 0);
        assert!(fixture.0.lock().unwrap().cancelled_jobs.is_empty());
        drop(server);
    }
}

#[tokio::test]
async fn ordinary_judge_recovers_enqueue_headers_after_the_total_deadline_before_cancelling() {
    for mode in [LlmMode::Local, LlmMode::External] {
        use coding_agent::semantic_judge::{RegisteredSemanticJudge, SemanticJudgePolicy};
        let fixture = ordinary_judge_fixture(mode, b"{}", RpcDelivery::Complete);
        let server = start_jobworkerp_server(fixture.clone()).await;
        let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
            .await
            .unwrap();
        let judge = RegisteredSemanticJudge::observe(&handle, mode)
            .await
            .unwrap()
            .with_policy(SemanticJudgePolicy {
                max_retries: 0,
                deadline_seconds: 10,
            })
            .unwrap();
        let gate = Arc::new(tokio::sync::Notify::new());
        {
            let mut calls = fixture.0.lock().unwrap();
            calls.worker_lookup_delay = Duration::from_millis(1200);
            calls.enqueue_response_gate = Some(gate.clone());
        }
        let task = tokio::spawn(async move {
            judge
                .begin(&handle, b"judge", b"source", "delayed:judge")
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while fixture.enqueue_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(9000)).await;
        assert!(
            !task.is_finished(),
            "an accepted enqueue must retain ownership of its response"
        );
        gate.notify_one();
        assert!(task.await.unwrap().is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.0.lock().unwrap().cancelled_jobs.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fixture.0.lock().unwrap().cancelled_jobs, vec![RPC_JOB_ID]);
        assert_eq!(fixture.enqueue_count(), 1);
        drop(server);
    }
}

#[tokio::test]
async fn ordinary_judge_total_deadline_starts_at_admission_and_cancels_the_actual_job() {
    for mode in [LlmMode::Local, LlmMode::External] {
        use coding_agent::semantic_judge::{RegisteredSemanticJudge, SemanticJudgePolicy};
        let fixture = ordinary_judge_fixture(mode, b"{}", RpcDelivery::Complete);
        let server = start_jobworkerp_server(fixture.clone()).await;
        let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
            .await
            .unwrap();
        let judge = RegisteredSemanticJudge::observe(&handle, mode)
            .await
            .unwrap()
            .with_policy(SemanticJudgePolicy {
                max_retries: 0,
                deadline_seconds: 10,
            })
            .unwrap();
        let execution = judge
            .begin(&handle, b"judge", b"source", "deadline:judge")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10_100)).await;
        assert!(execution.finish().await.is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.0.lock().unwrap().cancelled_jobs.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fixture.0.lock().unwrap().cancelled_jobs, vec![RPC_JOB_ID]);
        assert_eq!(fixture.enqueue_count(), 1);
        assert_eq!(
            fixture
                .captured_job_request()
                .overrides
                .unwrap()
                .retry_policy
                .unwrap()
                .r#type,
            0
        );
        drop(server);
    }
}

#[tokio::test]
async fn ordinary_judge_rejects_changed_workers_and_incomplete_streams_without_redispatch() {
    use coding_agent::semantic_judge::RegisteredSemanticJudge;
    for changed in [false, true] {
        let fixture =
            ordinary_judge_fixture(LlmMode::Local, b"{}", RpcDelivery::LoseStreamAfterEnqueue);
        let server = start_jobworkerp_server(fixture.clone()).await;
        let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
            .await
            .unwrap();
        let judge = RegisteredSemanticJudge::observe(&handle, LlmMode::Local)
            .await
            .unwrap();
        if changed {
            fixture.set_worker_mutation(2);
        }
        assert!(
            judge
                .evaluate(&handle, b"judge", b"source", "ordinary-run:attempt:judge")
                .await
                .is_err()
        );
        assert_eq!(fixture.enqueue_count(), usize::from(!changed));
        assert!(fixture.0.lock().unwrap().created_worker.is_none());
        drop(server);
    }
}

#[tokio::test]
async fn dropping_an_admitted_ordinary_judge_cancels_its_actual_job() {
    for mode in [LlmMode::Local, LlmMode::External] {
        use coding_agent::semantic_judge::RegisteredSemanticJudge;
        let fixture = ordinary_judge_fixture(mode, b"{}", RpcDelivery::Complete);
        let server = start_jobworkerp_server(fixture.clone()).await;
        let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
            .await
            .unwrap();
        let judge = RegisteredSemanticJudge::observe(&handle, mode)
            .await
            .unwrap();
        let execution = judge
            .begin(&handle, b"judge", b"source", "run:attempt:judge")
            .await
            .unwrap();
        assert_eq!(execution.job_id(), Some(RPC_JOB_ID));
        drop(execution);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if fixture.0.lock().unwrap().cancelled_jobs == vec![RPC_JOB_ID] {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fixture.enqueue_count(), 1);
        drop(server);
    }
}

#[tokio::test]
async fn ordinary_judge_requires_the_actual_caller_end_even_after_a_collected_pass_response() {
    use coding_agent::semantic_judge::RegisteredSemanticJudge;
    let fixture = ordinary_judge_fixture(
        LlmMode::Local,
        br#"{"outcome":"PASS"}"#,
        RpcDelivery::CollectedWithoutEnd,
    );
    let server = start_jobworkerp_server(fixture.clone()).await;
    let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
        .await
        .unwrap();
    let judge = RegisteredSemanticJudge::observe(&handle, LlmMode::Local)
        .await
        .unwrap();
    assert!(
        judge
            .evaluate(&handle, b"judge", b"source", "run:attempt:judge")
            .await
            .is_err()
    );
    assert_eq!(fixture.enqueue_count(), 1);
    drop(server);
}

// Both the typed `Trailer.error` and its deprecated JSON mirror fail the run.
#[tokio::test]
async fn ordinary_judge_rejects_an_error_trailer_even_after_terminal_pass() {
    use coding_agent::semantic_judge::RegisteredSemanticJudge;
    for delivery in [
        RpcDelivery::CompleteWithErrorTrailer,
        RpcDelivery::CompleteWithTypedErrorTrailer,
    ] {
        let fixture = ordinary_judge_fixture(LlmMode::Local, br#"{"outcome":"PASS"}"#, delivery);
        let server = start_jobworkerp_server(fixture.clone()).await;
        let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
            .await
            .unwrap();
        let judge = RegisteredSemanticJudge::observe(&handle, LlmMode::Local)
            .await
            .unwrap();
        assert!(
            judge
                .evaluate(&handle, b"judge", b"source", "run:attempt:judge")
                .await
                .is_err(),
            "{delivery:?}"
        );
        assert_eq!(fixture.enqueue_count(), 1);
    }
}

#[tokio::test]
async fn cancelling_ordinary_judge_before_its_enqueue_response_stops_the_accepted_job() {
    for mode in [LlmMode::Local, LlmMode::External] {
        use coding_agent::semantic_judge::RegisteredSemanticJudge;
        let fixture = ordinary_judge_fixture(mode, b"{}", RpcDelivery::Complete);
        let gate = Arc::new(tokio::sync::Notify::new());
        fixture.0.lock().unwrap().enqueue_response_gate = Some(gate.clone());
        let server = start_jobworkerp_server(fixture.clone()).await;
        let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
            .await
            .unwrap();
        let judge = RegisteredSemanticJudge::observe(&handle, mode)
            .await
            .unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        let owned_token = token.clone();
        let task = tokio::spawn(async move {
            judge
                .begin_cancellable(
                    &handle,
                    b"judge",
                    b"source",
                    "run:attempt:judge",
                    owned_token,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.enqueue_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        token.cancel();
        gate.notify_one();
        assert!(task.await.unwrap().is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if fixture.0.lock().unwrap().cancelled_jobs == vec![RPC_JOB_ID] {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fixture.enqueue_count(), 1);
        drop(server);
    }
}

#[tokio::test]
async fn cancelling_external_judge_while_waiting_for_a_final_result_cancels_without_reenqueue() {
    use coding_agent::semantic_judge::RegisteredSemanticJudge;
    let fixture = ordinary_judge_fixture(LlmMode::External, b"{}", RpcDelivery::Complete);
    fixture.0.lock().unwrap().empty_result_lookups = usize::MAX;
    let server = start_jobworkerp_server(fixture.clone()).await;
    let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
        .await
        .unwrap();
    let judge = RegisteredSemanticJudge::observe(&handle, LlmMode::External)
        .await
        .unwrap();
    let token = tokio_util::sync::CancellationToken::new();
    let execution = judge
        .begin_cancellable(&handle, b"judge", b"source", "pending:judge", token.clone())
        .await
        .unwrap();
    let task = tokio::spawn(execution.finish());
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.result_lookup_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    token.cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.0.lock().unwrap().cancelled_jobs.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.0.lock().unwrap().cancelled_jobs, vec![RPC_JOB_ID]);
    assert_eq!(fixture.enqueue_count(), 1);
}

fn ordinary_configured_judge_fixture(settings: &LlmSettings) -> JudgeRpcFixture {
    let fixture = ordinary_judge_fixture(settings.mode, b"{}", RpcDelivery::Complete);
    let mut calls = fixture.0.lock().unwrap();
    let value = if settings.mode == LlmMode::Local {
        calls.runner.runner_settings_proto="syntax = \"proto3\"; package llama_cpp; message LlamaRunnerSettings { string model = 1; optional string hf_repo = 2; optional uint32 ctx_size = 3; }".into();
        let (model, hf_repo, ctx_size) =
            commands::llm_settings::resolve_local_llm_env_triple(settings, |_| None);
        serde_json::json!({"model":model,"hf_repo":hf_repo,"ctx_size":ctx_size})
    } else {
        serde_json::json!({"genai":{"model":settings.provider_model,"base_url":settings.base_url.clone().unwrap_or_default()}})
    };
    let descriptor = JobworkerpProto::parse_runner_settings_schema_descriptor(&calls.runner)
        .unwrap()
        .unwrap();
    calls
        .worker
        .as_mut()
        .unwrap()
        .data
        .as_mut()
        .unwrap()
        .runner_settings =
        JobworkerpProto::json_value_to_message(descriptor, &value, true, true).unwrap();
    drop(calls);
    fixture
}

#[tokio::test]
async fn ordinary_judge_checks_the_effective_model_settings_and_records_the_actual_configuration_digest()
 {
    use coding_agent::semantic_judge::RegisteredSemanticJudge;
    for settings in [
        LlmSettings {
            local_preset_id: Some("custom".into()),
            local_model_file: Some("chosen.gguf".into()),
            local_hf_repo: Some("org/chosen".into()),
            local_ctx_size: Some(8192),
            ..LlmSettings::default()
        },
        external_settings("gpt-4o", Some("https://llm.example/v1")),
    ] {
        let fixture = ordinary_configured_judge_fixture(&settings);
        let server = start_jobworkerp_server(fixture.clone()).await;
        let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
            .await
            .unwrap();
        let judge = RegisteredSemanticJudge::observe_for_settings(&handle, &settings)
            .await
            .unwrap();
        let digest = judge.configuration_digest();
        assert_eq!(digest.len(), 64);
        let mut changed = settings.clone();
        if settings.mode == LlmMode::Local {
            changed.local_model_file = Some("other.gguf".into());
        } else {
            changed.provider_model = Some("other-model".into());
        }
        assert!(
            RegisteredSemanticJudge::observe_for_settings(&handle, &changed)
                .await
                .is_err()
        );
        if settings.mode == LlmMode::External {
            changed = settings.clone();
            changed.base_url = Some("https://other.example/v1".into());
            assert!(
                RegisteredSemanticJudge::observe_for_settings(&handle, &changed)
                    .await
                    .is_err()
            );
        }
        assert_eq!(fixture.enqueue_count(), 0);
        drop(server);
    }
}

#[tokio::test]
async fn ordinary_local_judge_uses_effective_environment_overrides_only_without_a_saved_preset() {
    use coding_agent::semantic_judge::RegisteredSemanticJudge;
    let actual = LlmSettings {
        local_preset_id: Some("custom".into()),
        local_model_file: Some("override.gguf".into()),
        local_hf_repo: Some("org/override".into()),
        local_ctx_size: Some(12288),
        ..LlmSettings::default()
    };
    let fixture = ordinary_configured_judge_fixture(&actual);
    let server = start_jobworkerp_server(fixture.clone()).await;
    let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&server.endpoint)
        .await
        .unwrap();
    let environment = |name: &str| match name {
        "LOOKBACK_LLM_MODEL" => Some("override.gguf".into()),
        "LOOKBACK_LLM_HF_REPO" => Some("org/override".into()),
        "LOOKBACK_LLM_CTX_SIZE" => Some("12288".into()),
        _ => None,
    };
    let unspecified = LlmSettings::default();
    assert!(
        RegisteredSemanticJudge::observe_for_settings_with_env(&handle, &unspecified, environment)
            .await
            .is_ok()
    );
    assert!(
        RegisteredSemanticJudge::observe_for_settings_with_env(&handle, &unspecified, |_| None)
            .await
            .is_err()
    );
    assert!(
        RegisteredSemanticJudge::observe_for_settings_with_env(&handle, &actual, |_| Some(
            "ignored".into()
        ))
        .await
        .is_ok()
    );
    let selected =
        coding_agent::semantic_judge::provider_selection_with_env(&unspecified, environment)
            .unwrap();
    assert_eq!(selected.model_id, "override.gguf");
    assert_ne!(
        selected,
        coding_agent::semantic_judge::provider_selection_with_env(&unspecified, |_| None).unwrap()
    );
    assert_eq!(fixture.enqueue_count(), 0);
    drop(server);
}
