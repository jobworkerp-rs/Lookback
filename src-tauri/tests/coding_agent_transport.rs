use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use futures::{Stream, StreamExt};
use lookback_tauri_lib::{
    grpc::proto::jobworkerp::runner::sandbox::{
        SandboxExecExit, SandboxExecOutput, SandboxExecResult,
        sandbox_exec_result::Result as SandboxResult,
    },
    jobworkerp::{
        JobworkerpHandle,
        coding_agent::{CodingAgentEvent, CodingAgentTransportOptions},
    },
};
use prost::Message;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, transport::Server};

use jobworkerp_client::jobworkerp::{
    data::{
        FeedDataTransport, Job, JobId, MethodProtoMap, MethodSchema, ProblemDetails, ResponseType,
        ResultOutputItem, Runner, RunnerData, RunnerId, StreamError, StreamErrorCode,
        StreamingOutputType, Trailer, Worker, WorkerData, WorkerId, result_output_item::Item,
    },
    service::{
        self, ClientStreamRequest, CountCondition, CountResponse, CountRunnerRequest,
        CountWorkerRequest, CreateJobResponse, CreateRunnerRequest, CreateRunnerResponse,
        CreateWorkerResponse, FindListRequest, FindListWithProcessingStatusRequest,
        FindQueueListRequest, FindRunnerListRequest, FindWorkerListRequest, JobAndStatus,
        LoadWorkerRequest, OptionalJobResponse, OptionalRunnerResponse, OptionalWorkerResponse,
        ReleaseStaticWorkerRequest, RunnerNameRequest, SuccessResponse, WorkerNameRequest,
        client_stream_request, job_request,
        job_service_server::{JobService, JobServiceServer},
        runner_service_server::{RunnerService, RunnerServiceServer},
        worker_service_server::{WorkerService, WorkerServiceServer},
    },
};

const WORKER_ID: i64 = 71;
const RUNNER_ID: i64 = 19;
const JOB_ID: i64 = 843;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResultBehavior {
    Success,
    MissingEnd,
    EndWithoutExit,
    MalformedMessage,
    UnknownResultField,
    EmptyResult,
    StructuredStreamError,
    StructuredTimeout,
    StructuredCancelled,
    FutureVersionStreamError,
    NonJsonStreamError,
    UnusedLegacyKey,
    TypedStreamError,
    WorkerExecutionFailed,
    WorkerCancelled,
    WorkerTimeout,
    UnknownStreamErrorCode,
    EndThenGrpcStatus,
    EndThenData,
    EndThenEnd,
    NoGrpcClose,
    FullDuplex,
    EnqueueStatus,
}

#[derive(Default)]
struct Calls {
    worker_lookup: usize,
    runner_lookup: usize,
    client_stream_calls: usize,
    one_shot_calls: usize,
    unary_enqueue_calls: usize,
    delete_calls: usize,
    job_request: Option<service::JobRequest>,
    client_frames: Vec<ClientStreamRequest>,
}

#[derive(Clone)]
struct Fixture {
    calls: Arc<StdMutex<Calls>>,
    behavior: ResultBehavior,
    require_client_stream: bool,
    output_type: StreamingOutputType,
    response_type: ResponseType,
    result_proto: String,
    omit_job_id: bool,
    full_duplex_resume: Arc<Notify>,
}

impl Fixture {
    fn new(behavior: ResultBehavior, require_client_stream: bool) -> Self {
        Self {
            calls: Arc::new(StdMutex::new(Calls::default())),
            behavior,
            require_client_stream,
            output_type: StreamingOutputType::Streaming,
            response_type: ResponseType::Direct,
            result_proto: include_str!("../../proto/jobworkerp/runner/sandbox_result.proto")
                .to_owned(),
            omit_job_id: false,
            full_duplex_resume: Arc::new(Notify::new()),
        }
    }

    fn items(&self) -> Vec<ResultOutputItem> {
        let output = |stream: &str, data: &[u8]| ResultOutputItem {
            item: Some(Item::Data(
                SandboxExecResult {
                    result: Some(SandboxResult::Output(SandboxExecOutput {
                        stream: stream.to_owned(),
                        data: data.to_vec(),
                    })),
                }
                .encode_to_vec(),
            )),
        };
        let exit = |exit_code| ResultOutputItem {
            item: Some(Item::Data(
                SandboxExecResult {
                    result: Some(SandboxResult::Exit(SandboxExecExit {
                        exit_code,
                        execution_time_ms: 418,
                        sandbox_id: "sandbox-test".to_owned(),
                    })),
                }
                .encode_to_vec(),
            )),
        };
        let end = |metadata: HashMap<String, String>| ResultOutputItem {
            item: Some(Item::End(Trailer {
                metadata,
                ..Default::default()
            })),
        };
        let typed_end = |origin: &str, code: StreamErrorCode| ResultOutputItem {
            item: Some(Item::End(Trailer {
                error: Some(StreamError {
                    code: code as i32,
                    message: "AUTH_SECRET_FROM_TYPED_STREAM_ERROR".to_owned(),
                    origin: origin.to_owned(),
                    problem: Some(ProblemDetails {
                        r#type: "urn:sandbox".to_owned(),
                        detail: Some("AUTH_SECRET_FROM_TYPED_PROBLEM".to_owned()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            })),
        };
        let terminal = || {
            vec![
                ResultOutputItem {
                    item: Some(Item::Data(
                        SandboxExecResult {
                            result: Some(SandboxResult::Exit(SandboxExecExit {
                                exit_code: 23,
                                execution_time_ms: 418,
                                sandbox_id: "sandbox-test".to_owned(),
                            })),
                        }
                        .encode_to_vec(),
                    )),
                },
                end(HashMap::new()),
            ]
        };

        match self.behavior {
            ResultBehavior::Success => vec![
                output("stdout", b"out"),
                output("stdout", b"\0"),
                output("stderr", b"\xfferr"),
                exit(23),
                end(HashMap::new()),
            ],
            ResultBehavior::StructuredStreamError
            | ResultBehavior::StructuredTimeout
            | ResultBehavior::StructuredCancelled => {
                let (code, message) = match self.behavior {
                    ResultBehavior::StructuredStreamError => {
                        ("EXECUTION_FAILED", "AUTH_SECRET_FROM_STREAM_ERROR")
                    }
                    ResultBehavior::StructuredTimeout => {
                        ("TIMEOUT", "AUTH_SECRET_FROM_TIMEOUT_STREAM_ERROR")
                    }
                    ResultBehavior::StructuredCancelled => {
                        ("CANCELLED", "AUTH_SECRET_FROM_CANCELLED_STREAM_ERROR")
                    }
                    _ => unreachable!("only structured error cases are matched"),
                };
                let mut metadata = HashMap::new();
                metadata.insert(
                    "stream_error".to_owned(),
                    serde_json::json!({
                        "version": 1,
                        "code": code,
                        "message": message,
                        "origin": "SANDBOX",
                    })
                    .to_string(),
                );
                vec![exit(0), end(metadata)]
            }
            ResultBehavior::FutureVersionStreamError => {
                let mut metadata = HashMap::new();
                metadata.insert(
                    "stream_error".to_owned(),
                    r#"{"version":2,"code":"EXECUTION_FAILED","message":"AUTH_SECRET_FROM_STREAM_ERROR","origin":"SANDBOX"}"#.to_owned(),
                );
                vec![exit(0), end(metadata)]
            }
            ResultBehavior::NonJsonStreamError => {
                let mut metadata = HashMap::new();
                metadata.insert(
                    "stream_error".to_owned(),
                    "AUTH_SECRET_FROM_NON_JSON_STREAM_ERROR".to_owned(),
                );
                vec![exit(0), end(metadata)]
            }
            ResultBehavior::UnusedLegacyKey => {
                let mut metadata = HashMap::new();
                metadata.insert(
                    "jobworkerp.stream.error".to_owned(),
                    "AUTH_SECRET_FROM_LEGACY_STREAM_ERROR".to_owned(),
                );
                vec![exit(0), end(metadata)]
            }
            ResultBehavior::TypedStreamError => {
                vec![
                    exit(0),
                    typed_end("SANDBOX", StreamErrorCode::ExecutionFailed),
                ]
            }
            // The WORKER origin marks failures jobworkerp detects outside the
            // runner: a failure or cancel before the stream starts (no Data),
            // or a job timeout, which can also cut a running stream.
            ResultBehavior::WorkerExecutionFailed => {
                vec![typed_end("WORKER", StreamErrorCode::ExecutionFailed)]
            }
            ResultBehavior::WorkerCancelled => {
                vec![typed_end("WORKER", StreamErrorCode::Cancelled)]
            }
            ResultBehavior::WorkerTimeout => vec![
                output("stdout", b"partial"),
                typed_end("WORKER", StreamErrorCode::Timeout),
            ],
            ResultBehavior::UnknownStreamErrorCode => {
                let mut metadata = HashMap::new();
                metadata.insert(
                    "stream_error".to_owned(),
                    serde_json::json!({
                        "version": 1,
                        "code": "UNTRUSTED_UNKNOWN_CODE",
                        "message": "AUTH_SECRET_FROM_STREAM_ERROR",
                        "origin": "SANDBOX",
                    })
                    .to_string(),
                );
                vec![exit(0), end(metadata)]
            }
            ResultBehavior::EndThenGrpcStatus
            | ResultBehavior::EndThenData
            | ResultBehavior::EndThenEnd
            | ResultBehavior::NoGrpcClose => terminal(),
            ResultBehavior::FullDuplex => terminal(),
            ResultBehavior::EnqueueStatus => Vec::new(),
            ResultBehavior::MissingEnd => vec![output("stdout", b"partial"), exit(23)],
            ResultBehavior::EndWithoutExit => {
                vec![output("stdout", b"partial"), end(HashMap::new())]
            }
            ResultBehavior::MalformedMessage => vec![
                ResultOutputItem {
                    item: Some(Item::Data(vec![0x0f])),
                },
                end(HashMap::new()),
            ],
            ResultBehavior::UnknownResultField => vec![
                ResultOutputItem {
                    item: Some(Item::Data(vec![0x18, 0x01])),
                },
                end(HashMap::new()),
            ],
            ResultBehavior::EmptyResult => vec![
                ResultOutputItem {
                    item: Some(Item::Data(Vec::new())),
                },
                end(HashMap::new()),
            ],
        }
    }

    fn response_items(&self) -> Vec<Result<ResultOutputItem, Status>> {
        let mut items: Vec<_> = self.items().into_iter().map(Ok).collect();
        match self.behavior {
            ResultBehavior::EndThenGrpcStatus => {
                items.push(Err(Status::internal("AUTH_SECRET_FROM_GRPC_STATUS")));
            }
            ResultBehavior::EndThenData => {
                items.push(Ok(ResultOutputItem {
                    item: Some(Item::Data(
                        SandboxExecResult {
                            result: Some(SandboxResult::Output(SandboxExecOutput {
                                stream: "stdout".to_owned(),
                                data: b"after-end".to_vec(),
                            })),
                        }
                        .encode_to_vec(),
                    )),
                }));
            }
            ResultBehavior::EndThenEnd => {
                items.push(Ok(ResultOutputItem {
                    item: Some(Item::End(Trailer::default())),
                }));
            }
            _ => {}
        }
        items
    }

    fn response_metadata(&self) -> tonic::metadata::MetadataMap {
        let mut metadata = tonic::metadata::MetadataMap::new();
        if !self.omit_job_id {
            metadata.insert_bin(
                "x-job-id-bin",
                tonic::metadata::MetadataValue::from_bytes(
                    &JobId { value: JOB_ID }.encode_to_vec(),
                ),
            );
        }
        metadata
    }

    fn worker_data(&self) -> WorkerData {
        WorkerData {
            name: "sandbox-worker".to_owned(),
            runner_id: Some(RunnerId { value: RUNNER_ID }),
            response_type: self.response_type as i32,
            ..Default::default()
        }
    }

    fn runner_data(&self) -> RunnerData {
        let mut schemas = HashMap::new();
        let args_proto = actual_sandbox_args_proto();
        let method_schema = |require_client_stream, client_stream_data_proto| MethodSchema {
            args_proto: args_proto.clone(),
            result_proto: self.result_proto.clone(),
            description: None,
            output_type: self.output_type as i32,
            require_client_stream,
            client_stream_data_proto,
        };
        schemas.insert("run".to_owned(), method_schema(false, None));
        schemas.insert(
            "run_with_client".to_owned(),
            method_schema(self.require_client_stream, Some(String::new())),
        );
        RunnerData {
            name: "SANDBOX".to_owned(),
            method_proto_map: Some(MethodProtoMap { schemas }),
            ..Default::default()
        }
    }
}

#[derive(Clone)]
struct MockJobService(Fixture);

type RpcStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl JobService for MockJobService {
    type EnqueueForStreamStream = RpcStream<ResultOutputItem>;
    type EnqueueForResultStream = RpcStream<jobworkerp_client::jobworkerp::data::JobResult>;
    type EnqueueWithClientStreamStream = RpcStream<ResultOutputItem>;
    type FindListStream = RpcStream<Job>;
    type FindQueueListStream = RpcStream<JobAndStatus>;
    type FindListWithProcessingStatusStream = RpcStream<JobAndStatus>;

    async fn enqueue(
        &self,
        _request: Request<service::JobRequest>,
    ) -> Result<Response<CreateJobResponse>, Status> {
        self.0.calls.lock().unwrap().unary_enqueue_calls += 1;
        Err(Status::unimplemented("mock"))
    }

    async fn enqueue_for_result(
        &self,
        _request: Request<service::JobRequest>,
    ) -> Result<Response<Self::EnqueueForResultStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn enqueue_for_stream(
        &self,
        request: Request<service::JobRequest>,
    ) -> Result<Response<Self::EnqueueForStreamStream>, Status> {
        if self.0.behavior == ResultBehavior::EnqueueStatus {
            return Err(Status::internal("AUTH_SECRET_FROM_GRPC_STATUS"));
        }
        let mut calls = self.0.calls.lock().unwrap();
        calls.one_shot_calls += 1;
        calls.job_request = Some(request.into_inner());
        drop(calls);

        let response_items = self.0.response_items();
        let output = if self.0.behavior == ResultBehavior::NoGrpcClose {
            Box::pin(futures::stream::iter(response_items).chain(futures::stream::pending()))
                as Self::EnqueueForStreamStream
        } else {
            Box::pin(futures::stream::iter(response_items)) as Self::EnqueueForStreamStream
        };
        let mut response = Response::new(output);
        *response.metadata_mut() = self.0.response_metadata();
        Ok(response)
    }

    async fn enqueue_with_client_stream(
        &self,
        request: Request<tonic::Streaming<ClientStreamRequest>>,
    ) -> Result<Response<Self::EnqueueWithClientStreamStream>, Status> {
        let mut requests = request.into_inner();
        let first = requests
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("missing initial job request"))?;
        let Some(client_stream_request::Request::JobRequest(job_request)) = first.request.as_ref()
        else {
            return Err(Status::invalid_argument(
                "first client stream frame must contain a job request",
            ));
        };
        {
            let mut calls = self.0.calls.lock().unwrap();
            calls.client_stream_calls += 1;
            calls.job_request = Some(job_request.clone());
            calls.client_frames.push(first);
        }
        if self.0.behavior == ResultBehavior::EnqueueStatus {
            return Err(Status::internal("AUTH_SECRET_FROM_GRPC_STATUS"));
        }

        let fixture = self.0.clone();
        let (output_tx, output_rx) = mpsc::channel(8);
        tokio::spawn(async move {
            if fixture.behavior == ResultBehavior::FullDuplex {
                let output = ResultOutputItem {
                    item: Some(Item::Data(
                        SandboxExecResult {
                            result: Some(SandboxResult::Output(SandboxExecOutput {
                                stream: "stdout".to_owned(),
                                data: b"output-during-blocked-input".to_vec(),
                            })),
                        }
                        .encode_to_vec(),
                    )),
                };
                if output_tx.send(Ok(output)).await.is_err() {
                    return;
                }
                fixture.full_duplex_resume.notified().await;
            }
            while let Ok(Some(frame)) = requests.message().await {
                let is_final = matches!(
                    frame.request.as_ref(),
                    Some(client_stream_request::Request::FeedData(
                        FeedDataTransport { is_final: true, .. }
                    ))
                );
                fixture.calls.lock().unwrap().client_frames.push(frame);
                if is_final {
                    for item in fixture.response_items() {
                        if output_tx.send(item).await.is_err() {
                            return;
                        }
                    }
                    if fixture.behavior == ResultBehavior::NoGrpcClose {
                        std::future::pending::<()>().await;
                    }
                    return;
                }
            }
        });

        let mut response = Response::new(
            Box::pin(ReceiverStream::new(output_rx)) as Self::EnqueueWithClientStreamStream
        );
        *response.metadata_mut() = self.0.response_metadata();
        Ok(response)
    }

    async fn delete(&self, _request: Request<JobId>) -> Result<Response<SuccessResponse>, Status> {
        self.0.calls.lock().unwrap().delete_calls += 1;
        Ok(Response::new(SuccessResponse { is_success: true }))
    }

    async fn find(
        &self,
        _request: Request<JobId>,
    ) -> Result<Response<OptionalJobResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn find_list(
        &self,
        _request: Request<FindListRequest>,
    ) -> Result<Response<Self::FindListStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn find_queue_list(
        &self,
        _request: Request<FindQueueListRequest>,
    ) -> Result<Response<Self::FindQueueListStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn find_list_with_processing_status(
        &self,
        _request: Request<FindListWithProcessingStatusRequest>,
    ) -> Result<Response<Self::FindListWithProcessingStatusStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn count(
        &self,
        _request: Request<CountCondition>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }
}

#[derive(Clone)]
struct MockWorkerService(Fixture);

#[tonic::async_trait]
impl WorkerService for MockWorkerService {
    type FindListStream = RpcStream<Worker>;

    async fn create(
        &self,
        _request: Request<WorkerData>,
    ) -> Result<Response<CreateWorkerResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn upsert_by_name(
        &self,
        _request: Request<WorkerData>,
    ) -> Result<Response<CreateWorkerResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn update(&self, _request: Request<Worker>) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn delete(
        &self,
        _request: Request<WorkerId>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn find(
        &self,
        _request: Request<WorkerId>,
    ) -> Result<Response<OptionalWorkerResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn find_by_name(
        &self,
        _request: Request<WorkerNameRequest>,
    ) -> Result<Response<OptionalWorkerResponse>, Status> {
        self.0.calls.lock().unwrap().worker_lookup += 1;
        Ok(Response::new(OptionalWorkerResponse {
            data: Some(Worker {
                id: Some(WorkerId { value: WORKER_ID }),
                data: Some(self.0.worker_data()),
            }),
        }))
    }

    async fn find_list(
        &self,
        _request: Request<FindWorkerListRequest>,
    ) -> Result<Response<Self::FindListStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn count(
        &self,
        _request: Request<CountCondition>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn count_by(
        &self,
        _request: Request<CountWorkerRequest>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn find_channel_list(
        &self,
        _request: Request<jobworkerp_client::jobworkerp::data::Empty>,
    ) -> Result<Response<service::FindChannelListResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn release_static_worker(
        &self,
        _request: Request<ReleaseStaticWorkerRequest>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn load(
        &self,
        _request: Request<LoadWorkerRequest>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }
}

#[derive(Clone)]
struct MockRunnerService(Fixture);

#[tonic::async_trait]
impl RunnerService for MockRunnerService {
    type FindListStream = RpcStream<Runner>;
    type FindListByStream = RpcStream<Runner>;

    async fn create(
        &self,
        _request: Request<CreateRunnerRequest>,
    ) -> Result<Response<CreateRunnerResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn delete(
        &self,
        _request: Request<RunnerId>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn find(
        &self,
        _request: Request<RunnerId>,
    ) -> Result<Response<OptionalRunnerResponse>, Status> {
        self.0.calls.lock().unwrap().runner_lookup += 1;
        Ok(Response::new(OptionalRunnerResponse {
            data: Some(Runner {
                id: Some(RunnerId { value: RUNNER_ID }),
                data: Some(self.0.runner_data()),
            }),
        }))
    }

    async fn find_by_name(
        &self,
        _request: Request<RunnerNameRequest>,
    ) -> Result<Response<OptionalRunnerResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn find_list(
        &self,
        _request: Request<FindListRequest>,
    ) -> Result<Response<Self::FindListStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn count(
        &self,
        _request: Request<CountCondition>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn find_list_by(
        &self,
        _request: Request<FindRunnerListRequest>,
    ) -> Result<Response<Self::FindListByStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn count_by(
        &self,
        _request: Request<CountRunnerRequest>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }
}

struct MockServer {
    url: String,
    shutdown: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.join.abort();
    }
}

async fn start_mock_server(fixture: Fixture) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (incoming_tx, incoming_rx) = mpsc::channel::<std::io::Result<TcpStream>>(4);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            if incoming_tx.send(Ok(stream)).await.is_err() {
                break;
            }
        }
    });
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let service_fixture = fixture.clone();
    let join = tokio::spawn(async move {
        Server::builder()
            .add_service(JobServiceServer::new(MockJobService(
                service_fixture.clone(),
            )))
            .add_service(WorkerServiceServer::new(MockWorkerService(
                service_fixture.clone(),
            )))
            .add_service(RunnerServiceServer::new(MockRunnerService(service_fixture)))
            .serve_with_incoming_shutdown(ReceiverStream::new(incoming_rx), async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    MockServer {
        url: format!("http://{addr}"),
        shutdown: Some(shutdown_tx),
        join,
    }
}

fn options(max_input_chunk_bytes: usize) -> CodingAgentTransportOptions {
    CodingAgentTransportOptions {
        input_buffer_capacity: 1,
        max_input_chunk_bytes,
        terminal_drain_timeout: Duration::from_secs(5),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
struct TestSandboxArgs {
    #[prost(string, tag = "1")]
    command: String,
    #[prost(string, repeated, tag = "2")]
    args: Vec<String>,
    #[prost(string, optional, tag = "3")]
    working_dir: Option<String>,
    #[prost(btree_map = "string, string", tag = "4")]
    env: std::collections::BTreeMap<String, String>,
    #[prost(uint64, optional, tag = "6")]
    timeout_ms: Option<u64>,
    #[prost(bool, tag = "7")]
    treat_nonzero_as_error: bool,
    #[prost(int32, repeated, tag = "8")]
    success_exit_codes: Vec<i32>,
    #[prost(bytes = "vec", optional, tag = "9")]
    stdin: Option<Vec<u8>>,
    #[prost(bool, optional, tag = "10")]
    tty: Option<bool>,
}

fn actual_sandbox_args_proto() -> String {
    let args = include_str!("fixtures/jobworkerp-proto/sandbox_args.proto")
        .lines()
        .filter(|line| !line.trim_start().starts_with("import "))
        .collect::<Vec<_>>()
        .join("\n");
    let common = include_str!("fixtures/jobworkerp-proto/sandbox_common.proto")
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            !line.starts_with("syntax =") && !line.starts_with("package ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{args}\n{common}")
}

#[tokio::test]
async fn client_stream_sends_job_request_then_binary_feeds_and_final_frame() {
    let fixture = Fixture::new(ResultBehavior::Success, true);
    let server = start_mock_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut stream = handle
        .start_coding_agent_with_options(
            "sandbox-worker",
            serde_json::json!({
                "command": "execute",
                "args": ["--flag", "two words"],
                "working_dir": "/tmp/work",
                "env": {"MODE": "test"},
                "timeout_ms": 9001,
                "treat_nonzero_as_error": true,
                "success_exit_codes": [0, 2],
                "stdin": "AAEC/w==",
                "tty": true,
            }),
            options(4),
        )
        .await
        .unwrap();

    assert_eq!(stream.job_id().value, JOB_ID);
    let input = stream
        .input_handle()
        .expect("run_with_client accepts stdin");
    input.send_input(vec![0, 0xff, b'x']).await.unwrap();
    input.finish_input().await.unwrap();
    assert!(input.send_input(vec![1]).await.is_err());
    assert!(input.finish_input().await.is_err());

    let mut events = Vec::new();
    while let Some(event) = stream.next_event().await.unwrap() {
        events.push(event);
    }

    assert_eq!(
        events,
        vec![
            CodingAgentEvent::Output {
                stream: "stdout".to_owned(),
                data: b"out".to_vec(),
            },
            CodingAgentEvent::Output {
                stream: "stdout".to_owned(),
                data: b"\0".to_vec(),
            },
            CodingAgentEvent::Output {
                stream: "stderr".to_owned(),
                data: b"\xfferr".to_vec(),
            },
            CodingAgentEvent::Exit {
                exit_code: 23,
                execution_time_ms: 418,
                sandbox_id: "sandbox-test".to_owned(),
            },
        ]
    );

    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls.worker_lookup, 1);
    assert_eq!(calls.runner_lookup, 1);
    assert_eq!(calls.client_stream_calls, 1);
    assert_eq!(calls.unary_enqueue_calls, 0);
    assert_eq!(calls.delete_calls, 0, "stdin EOF must not cancel the job");
    let request = calls.job_request.as_ref().unwrap();
    assert!(matches!(
        request.worker.as_ref(),
        Some(job_request::Worker::WorkerId(WorkerId { value: WORKER_ID }))
    ));
    assert_eq!(request.using.as_deref(), Some("run_with_client"));
    assert_eq!(
        request.args,
        TestSandboxArgs {
            command: "execute".to_owned(),
            args: vec!["--flag".to_owned(), "two words".to_owned()],
            working_dir: Some("/tmp/work".to_owned()),
            env: std::collections::BTreeMap::from([("MODE".to_owned(), "test".to_owned())]),
            timeout_ms: Some(9001),
            treat_nonzero_as_error: true,
            success_exit_codes: vec![0, 2],
            stdin: Some(vec![0, 1, 2, 255]),
            tty: Some(true),
        }
        .encode_to_vec(),
        "job args must use the registered dynamic protobuf schema"
    );
    assert!(matches!(
        calls
            .client_frames
            .first()
            .and_then(|frame| frame.request.as_ref()),
        Some(client_stream_request::Request::JobRequest(_))
    ));
    let feeds: Vec<_> = calls.client_frames[1..]
        .iter()
        .map(|frame| match frame.request.as_ref().unwrap() {
            client_stream_request::Request::FeedData(feed) => feed.clone(),
            _ => panic!("only FeedData frames may follow the initial request"),
        })
        .collect();
    assert_eq!(
        feeds,
        vec![
            FeedDataTransport {
                data: vec![0, 0xff, b'x'],
                is_final: false,
            },
            FeedDataTransport {
                data: Vec::new(),
                is_final: true,
            },
        ]
    );
}

// jobworkerp never wrote this key, so it does not mark the End as failed.
#[tokio::test]
async fn an_end_with_only_the_unused_legacy_key_completes() {
    let fixture = Fixture::new(ResultBehavior::UnusedLegacyKey, false);
    let server = start_mock_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut stream = handle
        .start_coding_agent_once("sandbox-worker", serde_json::json!({"command": "execute"}))
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next_event().await.unwrap() {
        events.push(event);
    }
    assert!(matches!(
        events.last(),
        Some(CodingAgentEvent::Exit { exit_code: 0, .. })
    ));
}

#[tokio::test]
async fn one_shot_stream_uses_enqueue_for_stream_and_decodes_binary_result() {
    let fixture = Fixture::new(ResultBehavior::Success, false);
    let server = start_mock_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut stream = handle
        .start_coding_agent_once("sandbox-worker", serde_json::json!({"command": "execute"}))
        .await
        .unwrap();

    assert_eq!(stream.job_id().value, JOB_ID);
    assert!(stream.input_handle().is_none());
    assert!(stream.input_handle().is_none());
    let mut events = Vec::new();
    while let Some(event) = stream.next_event().await.unwrap() {
        events.push(event);
    }

    assert!(matches!(
        events.last(),
        Some(CodingAgentEvent::Exit { exit_code: 23, .. })
    ));
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls.one_shot_calls, 1);
    assert_eq!(calls.client_stream_calls, 0);
    assert_eq!(
        calls.job_request.as_ref().unwrap().using.as_deref(),
        Some("run")
    );
}

#[tokio::test]
async fn same_named_but_wire_incompatible_result_schema_is_rejected_before_legacy_enqueue() {
    let wrong_result_proto = r#"
        syntax = "proto3";
        package jobworkerp.runner;
        message SandboxExecResult {
          oneof result { SandboxExecOutput output = 1; SandboxExecExit exit = 2; }
        }
        message SandboxExecOutput { string stream = 1; bytes data = 2; }
        message SandboxExecExit {
          int32 exit_code = 4;
          uint64 execution_time_ms = 2;
          string sandbox_id = 3;
        }
    "#;

    for client_stream in [false, true] {
        let mut fixture = Fixture::new(ResultBehavior::Success, client_stream);
        fixture.result_proto = wrong_result_proto.to_owned();
        let server = start_mock_server(fixture.clone()).await;
        let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
        let result = if client_stream {
            handle
                .start_coding_agent_with_options(
                    "sandbox-worker",
                    serde_json::json!({"command": "execute"}),
                    options(8),
                )
                .await
        } else {
            handle
                .start_coding_agent_once(
                    "sandbox-worker",
                    serde_json::json!({"command": "execute"}),
                )
                .await
        };

        assert!(
            result.is_err(),
            "wire-incompatible result schema must fail closed"
        );
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.client_stream_calls, 0);
        assert_eq!(calls.one_shot_calls, 0);
        assert_eq!(calls.unary_enqueue_calls, 0);
    }
}

#[tokio::test]
async fn no_result_worker_is_rejected_before_enqueue() {
    let mut fixture = Fixture::new(ResultBehavior::Success, true);
    fixture.response_type = ResponseType::NoResult;
    let server = start_mock_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();

    let error = match handle
        .start_coding_agent_with_options(
            "sandbox-worker",
            serde_json::json!({"command": "execute"}),
            options(8),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("a no-result worker cannot produce the required output stream"),
    };

    assert!(error.to_string().contains("DIRECT"));
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls.client_stream_calls, 0);
    assert_eq!(calls.one_shot_calls, 0);
    assert_eq!(calls.unary_enqueue_calls, 0);
}

#[tokio::test]
async fn rejects_wrong_method_and_oversized_input_without_unary_fallback_or_echoing_data() {
    let mut fixture = Fixture::new(ResultBehavior::Success, false);
    let server = start_mock_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let error = match handle
        .start_coding_agent_with_options(
            "sandbox-worker",
            serde_json::json!({"command": "execute"}),
            options(2),
        )
        .await
    {
        Err(error) => error.to_string(),
        Ok(_) => panic!("a worker without client-stream support must fail preflight"),
    };
    assert!(error.contains("client streaming"));
    assert_eq!(fixture.calls.lock().unwrap().client_stream_calls, 0);
    assert_eq!(fixture.calls.lock().unwrap().unary_enqueue_calls, 0);

    fixture.require_client_stream = true;
    let server = start_mock_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut stream = handle
        .start_coding_agent_with_options(
            "sandbox-worker",
            serde_json::json!({"command": "execute"}),
            options(2),
        )
        .await
        .unwrap();
    let oversized = b"sensitive-payload";
    let input = stream.input_handle().expect("client stream input");
    let error = input.send_input(oversized.to_vec()).await.unwrap_err();
    assert!(!error.to_string().contains("sensitive-payload"));
    input.send_input(b"ok".to_vec()).await.unwrap();
    input.finish_input().await.unwrap();
    while stream.next_event().await.unwrap().is_some() {}

    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls.unary_enqueue_calls, 0);
    let feeds: Vec<_> = calls
        .client_frames
        .iter()
        .skip(1)
        .filter_map(|frame| match frame.request.as_ref() {
            Some(client_stream_request::Request::FeedData(feed)) => Some(feed),
            _ => None,
        })
        .collect();
    assert_eq!(feeds.len(), 2);
    assert_eq!(feeds[0].data, b"ok");
    assert!(feeds[1].is_final);
}

#[tokio::test]
async fn malformed_results_stream_errors_and_missing_terminal_end_fail() {
    for (behavior, expected, secret) in [
        (
            ResultBehavior::StructuredStreamError,
            "execution failed",
            "AUTH_SECRET_FROM_STREAM_ERROR",
        ),
        (
            ResultBehavior::StructuredTimeout,
            "timed out",
            "AUTH_SECRET_FROM_TIMEOUT_STREAM_ERROR",
        ),
        (
            ResultBehavior::StructuredCancelled,
            "cancelled",
            "AUTH_SECRET_FROM_CANCELLED_STREAM_ERROR",
        ),
        (
            ResultBehavior::FutureVersionStreamError,
            "execution failed",
            "AUTH_SECRET_FROM_STREAM_ERROR",
        ),
        (
            ResultBehavior::TypedStreamError,
            "execution failed",
            "AUTH_SECRET_FROM_TYPED",
        ),
        (
            ResultBehavior::WorkerExecutionFailed,
            "unrecognized stream error",
            "AUTH_SECRET_FROM_TYPED",
        ),
        (
            ResultBehavior::WorkerCancelled,
            "was cancelled",
            "AUTH_SECRET_FROM_TYPED",
        ),
        (
            ResultBehavior::WorkerTimeout,
            "timed out",
            "AUTH_SECRET_FROM_TYPED",
        ),
        (
            ResultBehavior::NonJsonStreamError,
            "unrecognized stream error",
            "AUTH_SECRET_FROM_NON_JSON_STREAM_ERROR",
        ),
        (
            ResultBehavior::UnknownStreamErrorCode,
            "unrecognized stream error",
            "AUTH_SECRET_FROM_STREAM_ERROR",
        ),
        (ResultBehavior::MissingEnd, "terminal", ""),
        (ResultBehavior::EndWithoutExit, "exit result", ""),
        (ResultBehavior::MalformedMessage, "malformed protobuf", ""),
        (
            ResultBehavior::UnknownResultField,
            "recognized result field",
            "",
        ),
        (ResultBehavior::EmptyResult, "result", ""),
    ] {
        let fixture = Fixture::new(behavior, true);
        let server = start_mock_server(fixture).await;
        let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
        let mut stream = handle
            .start_coding_agent_with_options(
                "sandbox-worker",
                serde_json::json!({"command": "execute"}),
                options(8),
            )
            .await
            .unwrap();
        stream.input_handle().unwrap().finish_input().await.unwrap();
        let mut final_error = None;
        while let Some(event) = stream.next_event().await.unwrap_or_else(|error| {
            final_error = Some(error);
            None
        }) {
            let _ = event;
        }
        let error = final_error.expect("invalid result stream must fail");
        assert!(
            error
                .to_string()
                .to_lowercase()
                .contains(&expected.to_lowercase()),
            "{error}"
        );
        if !secret.is_empty() {
            assert_safe_error(&error, secret);
        }
    }
}

#[tokio::test]
async fn a_successful_grpc_header_without_job_id_is_not_treated_as_accepted() {
    let mut fixture = Fixture::new(ResultBehavior::Success, true);
    fixture.omit_job_id = true;
    let server = start_mock_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let result = handle
        .start_coding_agent_with_options(
            "sandbox-worker",
            serde_json::json!({"command": "execute"}),
            options(8),
        )
        .await;

    assert!(result.is_err());
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls.client_stream_calls, 1);
    assert_eq!(
        calls.unary_enqueue_calls, 0,
        "unknown ack must not be retried"
    );
}

#[tokio::test]
async fn end_requires_clean_grpc_eof_and_rejects_any_post_end_item() {
    for behavior in [
        ResultBehavior::EndThenGrpcStatus,
        ResultBehavior::EndThenData,
        ResultBehavior::EndThenEnd,
    ] {
        let fixture = Fixture::new(behavior, false);
        let server = start_mock_server(fixture).await;
        let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
        let mut stream = handle
            .start_coding_agent_once("sandbox-worker", serde_json::json!({"command": "execute"}))
            .await
            .unwrap();

        let mut saw_failure = false;
        loop {
            match stream.next_event().await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    saw_failure = true;
                    let rendered = format!(
                        "{error} {error:?} {}",
                        serde_json::json!({"error": error.to_string()})
                    );
                    assert!(!rendered.contains("AUTH_SECRET_FROM_GRPC_STATUS"));
                    break;
                }
            }
        }
        assert!(saw_failure, "End followed by {behavior:?} must not succeed");
    }
}

#[tokio::test]
async fn end_without_grpc_eof_times_out_instead_of_reporting_success() {
    let fixture = Fixture::new(ResultBehavior::NoGrpcClose, false);
    let server = start_mock_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut stream = handle
        .start_coding_agent_once_with_options(
            "sandbox-worker",
            serde_json::json!({"command": "execute"}),
            CodingAgentTransportOptions {
                terminal_drain_timeout: Duration::from_millis(50),
                ..CodingAgentTransportOptions::default()
            },
        )
        .await
        .unwrap();

    loop {
        match stream.next_event().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("stream that never closes after End must fail"),
            Err(error) => {
                assert!(error.to_string().contains("did not close"));
                break;
            }
        }
    }
}

#[tokio::test]
async fn blocked_input_writer_does_not_prevent_output_drain_or_final_ordering() {
    let fixture = Fixture::new(ResultBehavior::FullDuplex, true);
    let server = start_mock_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let mut output = handle
        .start_coding_agent_with_options(
            "sandbox-worker",
            serde_json::json!({"command": "execute"}),
            CodingAgentTransportOptions {
                input_buffer_capacity: 1,
                max_input_chunk_bytes: 64 * 1024,
                terminal_drain_timeout: Duration::from_secs(5),
            },
        )
        .await
        .unwrap();
    let input = output.input_handle().unwrap();
    let (writer_started, writer_started_rx) = oneshot::channel();
    let writer = tokio::spawn(async move {
        let mut writer_started = Some(writer_started);
        for _ in 0..256 {
            input.send_input(vec![0x5a; 64 * 1024]).await?;
            if let Some(started) = writer_started.take() {
                let _ = started.send(());
            }
        }
        input.finish_input().await
    });

    tokio::time::timeout(Duration::from_secs(1), writer_started_rx)
        .await
        .expect("input writer should start")
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), async {
            while !writer.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err(),
        "the paused server must apply bounded input backpressure"
    );
    let output_event = tokio::time::timeout(Duration::from_secs(1), output.next_event())
        .await
        .expect("output polling must remain independent from the blocked input sender")
        .unwrap();
    assert_eq!(
        output_event,
        Some(CodingAgentEvent::Output {
            stream: "stdout".to_owned(),
            data: b"output-during-blocked-input".to_vec(),
        })
    );
    assert!(!writer.is_finished());

    fixture.full_duplex_resume.notify_one();
    tokio::time::timeout(Duration::from_secs(10), writer)
        .await
        .expect("resuming server feed drain should unblock the bounded writer")
        .unwrap()
        .unwrap();
    let mut exit_seen = false;
    while let Some(event) = output.next_event().await.unwrap() {
        exit_seen |= matches!(event, CodingAgentEvent::Exit { .. });
    }
    assert!(
        exit_seen,
        "the ordered final input frame should close stdin"
    );
}

#[tokio::test]
async fn external_status_and_dynamic_argument_errors_never_echo_secrets() {
    let secret = "AUTH_SECRET_FROM_UNTRUSTED_ERROR";
    let fixture = Fixture::new(ResultBehavior::EnqueueStatus, true);
    let server = start_mock_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let error = handle
        .start_coding_agent_with_options(
            "sandbox-worker",
            serde_json::json!({"command": "execute"}),
            options(8),
        )
        .await
        .unwrap_err();
    assert_safe_error(&error, "AUTH_SECRET_FROM_GRPC_STATUS");

    let fixture = Fixture::new(ResultBehavior::Success, true);
    let server = start_mock_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let error = handle
        .start_coding_agent_with_options(
            "sandbox-worker",
            serde_json::json!({"command": "execute", "timeout_ms": secret}),
            options(8),
        )
        .await
        .unwrap_err();
    assert_safe_error(&error, secret);
}

fn assert_safe_error(error: &(impl std::fmt::Display + std::fmt::Debug), secret: &str) {
    let rendered = format!(
        "{} {:?} {}",
        error,
        error,
        serde_json::json!({"error": error.to_string()})
    );
    assert!(
        !rendered.contains(secret),
        "unsafe error rendering: {rendered}"
    );
}
