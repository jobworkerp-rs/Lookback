//! Local tonic fixture for exercising the coordinator's claimed judge bridge.

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
};

use futures::{Stream, stream};
use jobworkerp_client::{
    command::job::JOB_ID_HEADER_NAME,
    jobworkerp::{
        data::{
            Empty, Job, JobId, JobResult, JobResultData, JobResultId, MethodProtoMap, MethodSchema,
            ResponseType, ResultOutput, ResultOutputItem, ResultStatus, Runner, RunnerData,
            RunnerId, StreamingOutputType, Trailer, Worker, WorkerData, WorkerId,
            result_output_item::Item,
        },
        service::{
            self, ClientStreamRequest, CountCondition, CountJobResultRequest, CountResponse,
            CountRunnerRequest, CountWorkerRequest, CreateJobResponse, CreateRunnerRequest,
            CreateRunnerResponse, CreateWorkerResponse, DeleteJobResultBulkRequest,
            DeleteJobResultBulkResponse, FindJobResultListRequest, FindListByJobIdRequest,
            FindListRequest, FindListWithProcessingStatusRequest, FindQueueListRequest,
            FindRunnerListRequest, FindWorkerListRequest, JobAndStatus, JobRequest,
            ListenByWorkerRequest, ListenRequest, LoadWorkerRequest, OptionalJobResponse,
            OptionalJobResultResponse, OptionalRunnerResponse, OptionalWorkerResponse,
            ReleaseStaticWorkerRequest, RunnerNameRequest, SuccessResponse, WorkerNameRequest,
            job_result_service_server::{JobResultService, JobResultServiceServer},
            job_service_server::{JobService, JobServiceServer},
            runner_service_server::{RunnerService, RunnerServiceServer},
            worker_service_server::{WorkerService, WorkerServiceServer},
        },
    },
};
use prost::Message;
use sha2::Digest;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Notify, Semaphore, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, metadata::MetadataValue, transport::Server};

use lookback_tauri_lib::grpc::proto::jobworkerp::runner::llm::{
    LlmChatArgs, LlmChatResult,
    llm_chat_args::message_content::Content as ChatArgsContent,
    llm_chat_result::{MessageContent, message_content::Content},
};

const RUNNER_ID: i64 = 19;
const WORKER_ID: i64 = 71;
const JOB_ID: i64 = 843;

type RpcStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

#[derive(Clone, Copy)]
enum Delivery {
    BlockResponse,
    LoseStreamAfterEnqueue,
}

#[derive(Default)]
struct Calls {
    worker: Option<Worker>,
    created_worker: Option<WorkerData>,
    enqueue_requests: Vec<JobRequest>,
    chat_args: Vec<LlmChatArgs>,
    job_result: Option<JobResult>,
    result_lookup_count: usize,
}

#[derive(Clone)]
pub struct JudgeRpcFixture {
    calls: Arc<Mutex<Calls>>,
    arrived: Arc<Notify>,
    release: Arc<Semaphore>,
    result_lookup_arrived: Arc<Notify>,
    result_lookup_release: Arc<Semaphore>,
    model: Arc<String>,
    delivery: Delivery,
}

impl JudgeRpcFixture {
    pub fn new(model: impl Into<String>) -> Self {
        Self::with_delivery(model, Delivery::BlockResponse)
    }

    pub fn new_recoverable(model: impl Into<String>) -> Self {
        Self::with_delivery(model, Delivery::LoseStreamAfterEnqueue)
    }

    fn with_delivery(model: impl Into<String>, delivery: Delivery) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Calls::default())),
            arrived: Arc::new(Notify::new()),
            release: Arc::new(Semaphore::new(0)),
            result_lookup_arrived: Arc::new(Notify::new()),
            result_lookup_release: Arc::new(Semaphore::new(0)),
            model: Arc::new(model.into()),
            delivery,
        }
    }

    pub async fn wait_until_enqueued(&self) {
        loop {
            let notified = self.arrived.notified();
            if self.enqueue_count() > 0 {
                return;
            }
            notified.await;
        }
    }

    pub fn release_response(&self) {
        self.release.add_permits(1);
    }

    pub async fn wait_until_result_lookups(&self, count: usize) {
        loop {
            let notified = self.result_lookup_arrived.notified();
            if self.result_lookup_count() >= count {
                return;
            }
            notified.await;
        }
    }

    pub fn release_result_lookup(&self) {
        self.result_lookup_release.add_permits(1);
    }

    pub fn result_lookup_count(&self) -> usize {
        self.calls
            .lock()
            .expect("RPC fixture poisoned")
            .result_lookup_count
    }

    pub fn enqueue_count(&self) -> usize {
        self.calls
            .lock()
            .expect("RPC fixture poisoned")
            .enqueue_requests
            .len()
    }

    pub fn captured_chat_args(&self) -> LlmChatArgs {
        self.calls
            .lock()
            .expect("RPC fixture poisoned")
            .chat_args
            .last()
            .expect("judge request has been captured")
            .clone()
    }

    fn runner(&self) -> Runner {
        let mut schemas = std::collections::HashMap::new();
        schemas.insert(
            "chat".to_owned(),
            MethodSchema {
                args_proto: include_str!("../../../proto/jobworkerp/runner/llm/chat_args.proto")
                    .to_owned(),
                result_proto: include_str!(
                    "../../../proto/jobworkerp/runner/llm/chat_result.proto"
                )
                .to_owned(),
                description: Some("LLM chat".to_owned()),
                output_type: StreamingOutputType::Streaming as i32,
                require_client_stream: false,
                client_stream_data_proto: None,
            },
        );
        let data = RunnerData {
            name: "LLM".to_owned(),
            runner_settings_proto: runner_settings_proto(),
            method_proto_map: Some(MethodProtoMap { schemas }),
            ..Default::default()
        };
        Runner {
            id: Some(RunnerId { value: RUNNER_ID }),
            data: Some(data),
        }
    }

    fn make_response(&self, args: &LlmChatArgs) -> ResultOutputItem {
        let user = args
            .messages
            .get(1)
            .and_then(|message| message.content.as_ref())
            .and_then(|content| content.content.as_ref())
            .and_then(|content| match content {
                ChatArgsContent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .expect("user message must be plain UTF-8 text");
        let input: serde_json::Value = serde_json::from_str(user).expect("approved payload JSON");
        let criterion_results = input["criteria"]
            .as_array()
            .expect("approved criteria")
            .iter()
            .map(|criterion| {
                serde_json::json!({
                    "criterion_id": criterion["criterion_id"],
                    "result": "PASS",
                    "reason": "The approved report contains the required evidence.",
                    "evidence_refs": input["evidence"].as_array().unwrap().iter()
                        .map(|source| source["identity"].clone()).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "assessment_id": input["assessment_id"],
            "run_id": input["run_id"],
            "attempt_id": input["attempt_id"],
            "input_digest": format!("{:x}", sha2::Sha256::digest(user.as_bytes())),
            "criteria": criterion_results,
            "overall": "PASS",
        }))
        .expect("judge response JSON");
        let result = LlmChatResult {
            content: Some(MessageContent {
                content: Some(Content::Text(String::from_utf8(bytes).unwrap())),
            }),
            usage: Some(
                lookback_tauri_lib::grpc::proto::jobworkerp::runner::llm::llm_chat_result::Usage {
                    model: self.model.as_str().to_owned(),
                    ..Default::default()
                },
            ),
            done: true,
            ..Default::default()
        }
        .encode_to_vec();
        ResultOutputItem {
            item: Some(Item::Data(result)),
        }
    }

    fn make_partial_response(&self) -> ResultOutputItem {
        let result = LlmChatResult {
            content: Some(MessageContent {
                content: Some(Content::Text("partial".to_owned())),
            }),
            usage: Some(
                lookback_tauri_lib::grpc::proto::jobworkerp::runner::llm::llm_chat_result::Usage {
                    model: self.model.as_str().to_owned(),
                    ..Default::default()
                },
            ),
            done: false,
            ..Default::default()
        }
        .encode_to_vec();
        ResultOutputItem {
            item: Some(Item::Data(result)),
        }
    }
}

fn runner_settings_proto() -> String {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "coding_agent_judge_runner_settings.schema.json"
    ))
    .expect("checked-in runner settings fixture");
    fixture["runner_settings_proto_lines"]
        .as_array()
        .expect("runner settings proto lines")
        .iter()
        .map(|line| line.as_str().expect("proto line"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[tonic::async_trait]
impl RunnerService for JudgeRpcFixture {
    type FindListStream = RpcStream<Runner>;
    type FindListByStream = RpcStream<Runner>;

    async fn create(
        &self,
        _: Request<CreateRunnerRequest>,
    ) -> Result<Response<CreateRunnerResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn delete(&self, _: Request<RunnerId>) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn find(&self, _: Request<RunnerId>) -> Result<Response<OptionalRunnerResponse>, Status> {
        Ok(Response::new(OptionalRunnerResponse {
            data: Some(self.runner()),
        }))
    }
    async fn find_by_name(
        &self,
        request: Request<RunnerNameRequest>,
    ) -> Result<Response<OptionalRunnerResponse>, Status> {
        let data = (request.into_inner().name == "LLM").then(|| self.runner());
        Ok(Response::new(OptionalRunnerResponse { data }))
    }
    async fn find_list(
        &self,
        _: Request<FindListRequest>,
    ) -> Result<Response<Self::FindListStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn count(&self, _: Request<CountCondition>) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn find_list_by(
        &self,
        _: Request<FindRunnerListRequest>,
    ) -> Result<Response<Self::FindListByStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn count_by(
        &self,
        _: Request<CountRunnerRequest>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused"))
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
        self.calls.lock().expect("RPC fixture poisoned").worker = Some(Worker {
            id: Some(WorkerId { value: WORKER_ID }),
            data: Some(data.clone()),
        });
        self.calls
            .lock()
            .expect("RPC fixture poisoned")
            .created_worker = Some(data);
        Ok(Response::new(CreateWorkerResponse {
            id: Some(WorkerId { value: WORKER_ID }),
        }))
    }
    async fn upsert_by_name(
        &self,
        _: Request<WorkerData>,
    ) -> Result<Response<CreateWorkerResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn update(&self, _: Request<Worker>) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn delete(&self, _: Request<WorkerId>) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn find(&self, _: Request<WorkerId>) -> Result<Response<OptionalWorkerResponse>, Status> {
        Ok(Response::new(OptionalWorkerResponse {
            data: self
                .calls
                .lock()
                .expect("RPC fixture poisoned")
                .worker
                .clone(),
        }))
    }
    async fn find_by_name(
        &self,
        request: Request<WorkerNameRequest>,
    ) -> Result<Response<OptionalWorkerResponse>, Status> {
        let name = request.into_inner().name;
        let data = self
            .calls
            .lock()
            .expect("RPC fixture poisoned")
            .worker
            .clone()
            .filter(|worker| worker.data.as_ref().is_some_and(|data| data.name == name));
        Ok(Response::new(OptionalWorkerResponse { data }))
    }
    async fn find_list(
        &self,
        _: Request<FindWorkerListRequest>,
    ) -> Result<Response<Self::FindListStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn count(&self, _: Request<CountCondition>) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn count_by(
        &self,
        _: Request<CountWorkerRequest>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn find_channel_list(
        &self,
        _: Request<Empty>,
    ) -> Result<Response<service::FindChannelListResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn release_static_worker(
        &self,
        _: Request<ReleaseStaticWorkerRequest>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn load(
        &self,
        _: Request<LoadWorkerRequest>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused"))
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

    async fn enqueue(&self, _: Request<JobRequest>) -> Result<Response<CreateJobResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn enqueue_for_result(
        &self,
        _request: Request<JobRequest>,
    ) -> Result<Response<Self::EnqueueForResultStream>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn enqueue_for_stream(
        &self,
        request: Request<JobRequest>,
    ) -> Result<Response<Self::EnqueueForStreamStream>, Status> {
        let request = request.into_inner();
        let args_message = LlmChatArgs::decode(request.args.as_slice())
            .map_err(|_| Status::invalid_argument("not an LLM chat args message"))?;
        let user = args_message
            .messages
            .get(1)
            .and_then(|message| message.content.as_ref())
            .and_then(|content| content.content.as_ref());
        if user.is_none() {
            return Err(Status::invalid_argument("missing user message"));
        }
        let full_response = self.make_response(&args_message);
        let full_output = match full_response.item.as_ref() {
            Some(Item::Data(bytes)) => bytes.clone(),
            _ => return Err(Status::internal("fixture response was not data")),
        };
        let complete_output = vec![
            full_response,
            ResultOutputItem {
                item: Some(Item::End(Trailer::default())),
            },
        ];
        {
            let mut calls = self.calls.lock().expect("RPC fixture poisoned");
            calls.enqueue_requests.push(request.clone());
            calls.chat_args.push(args_message.clone());
            let worker = calls
                .worker
                .clone()
                .expect("worker was created before enqueue");
            let worker_data = worker.data.as_ref().expect("created worker has data");
            calls.job_result = Some(JobResult {
                id: Some(JobResultId { value: JOB_ID }),
                data: Some(JobResultData {
                    job_id: Some(JobId { value: JOB_ID }),
                    worker_id: worker.id,
                    worker_name: worker_data.name.clone(),
                    args: request.args.clone(),
                    uniq_key: request.uniq_key.clone(),
                    status: ResultStatus::Success as i32,
                    output: Some(ResultOutput { items: full_output }),
                    response_type: ResponseType::Direct as i32,
                    store_success: true,
                    store_failure: true,
                    using: Some("chat".to_owned()),
                    broadcast_results: true,
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        self.arrived.notify_one();
        let items = match self.delivery {
            Delivery::BlockResponse => {
                self.release
                    .acquire()
                    .await
                    .map_err(|_| Status::cancelled("fixture closed"))?
                    .forget();
                complete_output.into_iter().map(Ok).collect::<Vec<_>>()
            }
            Delivery::LoseStreamAfterEnqueue => vec![
                Ok(self.make_partial_response()),
                Err(Status::unavailable(
                    "fixture dropped stream after accepting job",
                )),
            ],
        };
        let mut response =
            Response::new(Box::pin(stream::iter(items)) as Self::EnqueueForStreamStream);
        let encoded_job_id = JobId { value: JOB_ID }.encode_to_vec();
        response.metadata_mut().insert_bin(
            JOB_ID_HEADER_NAME,
            MetadataValue::from_bytes(&encoded_job_id),
        );
        Ok(response)
    }
    async fn enqueue_with_client_stream(
        &self,
        _: Request<tonic::Streaming<ClientStreamRequest>>,
    ) -> Result<Response<Self::EnqueueWithClientStreamStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn delete(&self, _: Request<JobId>) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn find(&self, _: Request<JobId>) -> Result<Response<OptionalJobResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn find_list(
        &self,
        _: Request<FindListRequest>,
    ) -> Result<Response<Self::FindListStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn find_queue_list(
        &self,
        _: Request<FindQueueListRequest>,
    ) -> Result<Response<Self::FindQueueListStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn find_list_with_processing_status(
        &self,
        _: Request<FindListWithProcessingStatusRequest>,
    ) -> Result<Response<Self::FindListWithProcessingStatusStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn count(&self, _: Request<CountCondition>) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused"))
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

    async fn delete(&self, _: Request<JobResultId>) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn find(
        &self,
        _: Request<JobResultId>,
    ) -> Result<Response<OptionalJobResultResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn find_list(
        &self,
        _: Request<FindListRequest>,
    ) -> Result<Response<Self::FindListStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn find_list_by_job_id(
        &self,
        _: Request<FindListByJobIdRequest>,
    ) -> Result<Response<Self::FindListByJobIdStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn listen(&self, _: Request<ListenRequest>) -> Result<Response<JobResult>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn listen_stream(
        &self,
        _: Request<ListenRequest>,
    ) -> Result<Response<Self::ListenStreamStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn listen_by_worker(
        &self,
        _: Request<ListenByWorkerRequest>,
    ) -> Result<Response<Self::ListenByWorkerStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn count(&self, _: Request<CountCondition>) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn find_list_by(
        &self,
        _: Request<FindJobResultListRequest>,
    ) -> Result<Response<Self::FindListByStream>, Status> {
        let (lookup_number, job_result) = {
            let mut calls = self.calls.lock().expect("RPC fixture poisoned");
            calls.result_lookup_count += 1;
            (calls.result_lookup_count, calls.job_result.clone())
        };
        self.result_lookup_arrived.notify_one();
        if lookup_number == 1 {
            return Err(Status::unavailable("first result lookup is unavailable"));
        }
        if lookup_number == 2 {
            self.result_lookup_release
                .acquire()
                .await
                .map_err(|_| Status::cancelled("fixture closed"))?
                .forget();
        }
        let results = job_result.into_iter().map(Ok).collect::<Vec<_>>();
        Ok(Response::new(
            Box::pin(stream::iter(results)) as Self::FindListByStream
        ))
    }
    async fn count_by(
        &self,
        _: Request<CountJobResultRequest>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn delete_bulk(
        &self,
        _: Request<DeleteJobResultBulkRequest>,
    ) -> Result<Response<DeleteJobResultBulkResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
}

pub struct MockJobworkerpServer {
    pub endpoint: String,
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

pub async fn start_server(fixture: JudgeRpcFixture) -> MockJobworkerpServer {
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
