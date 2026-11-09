#![allow(dead_code)]

use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex},
};

use command_utils::protobuf::resolve::resolve_proto_imports;
use futures::Stream;
use jobworkerp_client::command::job::JOB_ID_HEADER_NAME;
use jobworkerp_client::jobworkerp::{
    data::{
        FeedDataTransport, Job, JobId, MethodProtoMap, MethodSchema, ResultOutputItem, Runner,
        RunnerData, RunnerId, StreamingOutputType, Worker, WorkerData, WorkerId,
    },
    service::{
        ClientStreamRequest, CountCondition, CountResponse, CountRunnerRequest, CountWorkerRequest,
        CreateRunnerRequest, CreateRunnerResponse, CreateWorkerResponse, FindListRequest,
        FindRunnerListRequest, FindWorkerListRequest, JobAndStatus, JobRequest,
        OptionalRunnerResponse, OptionalWorkerResponse, RunnerNameRequest, SuccessResponse,
        WorkerNameRequest, client_stream_request,
        job_service_server::{JobService, JobServiceServer},
        runner_service_server::{RunnerService, RunnerServiceServer},
        worker_service_server::{WorkerService, WorkerServiceServer},
    },
};
use prost::Message;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, transport::Server};

const SANDBOX_RUNNER_ID: i64 = 17;
const FIRST_WORKER_ID: i64 = 80;

type RpcStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

const SETTINGS_PROTO: &str = include_str!("jobworkerp-proto/sandbox_settings.proto");
const COMMON_PROTO: &str = include_str!("jobworkerp-proto/sandbox_common.proto");
const CLIENT_ARGS_PROTO: &str = include_str!("jobworkerp-proto/sandbox_args.proto");
const RESULT_PROTO: &str = include_str!("jobworkerp-proto/sandbox_result.proto");
const COMMON_PROTO_IMPORT: &str = "jobworkerp/runner/sandbox_common.proto";

fn resolved_sandbox_schema(source: &str) -> String {
    resolve_proto_imports(source, &[(COMMON_PROTO_IMPORT, COMMON_PROTO)])
        .expect("actual SANDBOX schema imports resolve")
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RpcCounts {
    pub runner_lookups: usize,
    pub worker_lookups: usize,
    pub create_calls: usize,
    pub upsert_calls: usize,
    pub worker_delete_calls: usize,
    pub release_static_calls: usize,
    pub job_enqueue_calls: usize,
    pub enqueue_for_stream_calls: usize,
    pub enqueue_with_client_stream_calls: usize,
    pub job_delete_calls: usize,
}

#[derive(Default)]
struct State {
    worker_lookup_gate: Option<(usize, OutputGate)>,
    trailing_stream_error: bool,
    allow_workflow_upsert: bool,
    workers: HashMap<String, Worker>,
    created_workers: Vec<WorkerData>,
    counts: RpcCounts,
    create_behavior: CreateBehavior,
    fail_next_worker_lookup: bool,
    malformed_next_worker_lookup: bool,
    fail_next_runner_lookup: bool,
    malformed_next_runner_lookup: bool,
    replace_worker_id: Option<WorkerId>,
    change_after_workflow_runner_lookup: Option<(String, bool)>,
    enqueue_behavior: EnqueueBehavior,
    enqueue_gate: Option<EnqueueGate>,
    output_gate: Option<OutputGate>,
    job_requests: Vec<JobRequest>,
    cancelled_job_ids: Vec<JobId>,
    grpc_timeout_headers: Vec<Option<String>>,
    feed_frames: Vec<FeedDataTransport>,
    job_result_items: Vec<ResultOutputItem>,
}

#[derive(Clone)]
pub struct EnqueueGate {
    entered: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

impl EnqueueGate {
    pub async fn wait_until_entered(&self) {
        self.entered.notified().await;
    }

    pub fn resume(&self) {
        self.resume.notify_one();
    }
}

#[derive(Clone)]
pub struct OutputGate {
    entered: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

impl OutputGate {
    pub async fn wait_until_entered(&self) {
        self.entered.notified().await;
    }

    pub fn resume(&self) {
        self.resume.notify_one();
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CreateBehavior {
    #[default]
    Normal,
    RaceWithMatchingWorker,
    AlreadyExistsWithoutWorker,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EnqueueBehavior {
    #[default]
    Normal,
    Unavailable,
    MissingJobId,
    InvalidJobId,
}

#[derive(Clone)]
pub struct RpcFixture {
    runner_data: Arc<Mutex<RunnerData>>,
    preparation_workflow_runner: Arc<Mutex<Option<RunnerData>>>,
    state: Arc<Mutex<State>>,
    input_notify: Arc<tokio::sync::Notify>,
}

impl Default for RpcFixture {
    fn default() -> Self {
        Self::with_runner_data(sandbox_runner_data())
    }
}

impl RpcFixture {
    pub fn pause_worker_lookup_after(&self, additional_lookups: usize) -> OutputGate {
        let gate = OutputGate {
            entered: Arc::new(tokio::sync::Notify::new()),
            resume: Arc::new(tokio::sync::Notify::new()),
        };
        let mut state = self.state.lock().unwrap();
        let target = state.counts.worker_lookups + additional_lookups + 1;
        state.worker_lookup_gate = Some((target, gate.clone()));
        gate
    }
    pub fn fail_stream_after_output(&self) {
        self.state.lock().unwrap().trailing_stream_error = true;
    }
    pub fn enable_preparation_workflow(&self) {
        let mut data = sandbox_runner_data();
        data.name = "WORKFLOW".into();
        data.runner_type = jobworkerp_client::jobworkerp::data::RunnerType::Workflow as i32;
        data.runner_settings_proto =
            include_str!("jobworkerp-proto/workflow_runner_settings.proto").into();
        data.method_proto_map = Some(MethodProtoMap {
            schemas: HashMap::from([(
                "run".into(),
                MethodSchema {
                    args_proto: include_str!("jobworkerp-proto/workflow_run_args.proto").into(),
                    result_proto: include_str!("jobworkerp-proto/workflow_result.proto").into(),
                    description: None,
                    output_type: StreamingOutputType::Both as i32,
                    require_client_stream: false,
                    client_stream_data_proto: None,
                },
            )]),
        });
        *self.preparation_workflow_runner.lock().unwrap() = Some(data);
        self.allow_workflow_registration();
    }

    pub fn mutate_workflow_runner(&self, mutate: impl FnOnce(&mut RunnerData)) {
        mutate(
            self.preparation_workflow_runner
                .lock()
                .unwrap()
                .as_mut()
                .expect("workflow runner enabled"),
        );
    }

    pub fn workflow_runner_data(&self) -> RunnerData {
        self.preparation_workflow_runner
            .lock()
            .unwrap()
            .clone()
            .expect("workflow runner enabled")
    }

    pub fn allow_workflow_registration(&self) {
        self.state.lock().unwrap().allow_workflow_upsert = true;
    }
    pub fn with_runner_data(runner_data: RunnerData) -> Self {
        Self {
            runner_data: Arc::new(Mutex::new(runner_data)),
            preparation_workflow_runner: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(State::default())),
            input_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn runner_data(&self) -> RunnerData {
        self.runner_data.lock().unwrap().clone()
    }

    pub fn mutate_runner_data(&self, mutate: impl FnOnce(&mut RunnerData)) {
        mutate(&mut self.runner_data.lock().unwrap());
    }

    pub fn counts(&self) -> RpcCounts {
        self.state.lock().unwrap().counts.clone()
    }

    pub fn created_workers(&self) -> Vec<WorkerData> {
        self.state.lock().unwrap().created_workers.clone()
    }

    pub fn job_requests(&self) -> Vec<JobRequest> {
        self.state.lock().unwrap().job_requests.clone()
    }

    pub fn cancelled_job_ids(&self) -> Vec<JobId> {
        self.state.lock().unwrap().cancelled_job_ids.clone()
    }

    pub fn grpc_timeout_headers(&self) -> Vec<Option<String>> {
        self.state.lock().unwrap().grpc_timeout_headers.clone()
    }

    pub fn set_job_result_items(&self, items: Vec<ResultOutputItem>) {
        self.state.lock().unwrap().job_result_items = items;
    }

    pub fn set_enqueue_unavailable(&self) {
        self.state.lock().unwrap().enqueue_behavior = EnqueueBehavior::Unavailable;
    }

    pub fn block_next_enqueue(&self) -> EnqueueGate {
        let gate = EnqueueGate {
            entered: Arc::new(tokio::sync::Notify::new()),
            resume: Arc::new(tokio::sync::Notify::new()),
        };
        self.state.lock().unwrap().enqueue_gate = Some(gate.clone());
        gate
    }

    pub fn block_next_output(&self) -> OutputGate {
        let gate = OutputGate {
            entered: Arc::new(tokio::sync::Notify::new()),
            resume: Arc::new(tokio::sync::Notify::new()),
        };
        self.state.lock().unwrap().output_gate = Some(gate.clone());
        gate
    }

    pub fn omit_next_job_id(&self) {
        self.state.lock().unwrap().enqueue_behavior = EnqueueBehavior::MissingJobId;
    }

    pub fn return_invalid_next_job_id(&self) {
        self.state.lock().unwrap().enqueue_behavior = EnqueueBehavior::InvalidJobId;
    }

    pub fn replace_first_worker_id(&self, worker_id: WorkerId) {
        self.state.lock().unwrap().replace_worker_id = Some(worker_id);
    }

    pub fn fail_next_runner_lookup_with_unknown(&self) {
        self.state.lock().unwrap().fail_next_runner_lookup = true;
    }

    pub fn malform_next_runner_lookup(&self) {
        self.state.lock().unwrap().malformed_next_runner_lookup = true;
    }

    pub async fn wait_for_final_feed_frame(&self) -> Vec<FeedDataTransport> {
        loop {
            let notified = self.input_notify.notified();
            let frames = self.state.lock().unwrap().feed_frames.clone();
            if frames.iter().any(|frame| frame.is_final) {
                return frames;
            }
            notified.await;
        }
    }

    pub fn first_worker_data(&self) -> WorkerData {
        let state = self.state.lock().unwrap();
        let name = &state.created_workers.first().expect("worker exists").name;
        state
            .workers
            .get(name)
            .and_then(|worker| worker.data.clone())
            .expect("worker data exists")
    }

    pub fn change_worker_after_workflow_runner_lookup(&self, name: &str, remove: bool) {
        self.state
            .lock()
            .unwrap()
            .change_after_workflow_runner_lookup = Some((name.into(), remove));
    }

    pub fn mutate_named_worker(&self, name: &str, mutate: impl FnOnce(&mut Worker)) {
        mutate(
            self.state
                .lock()
                .unwrap()
                .workers
                .get_mut(name)
                .expect("worker exists"),
        );
    }

    pub fn mutate_first_worker(&self, mutate: impl FnOnce(&mut WorkerData)) {
        let mut state = self.state.lock().unwrap();
        let name = state
            .created_workers
            .first()
            .expect("worker exists")
            .name
            .clone();
        let worker = state.workers.get_mut(&name).expect("worker exists");
        mutate(worker.data.as_mut().expect("worker data exists"));
    }

    pub fn simulate_create_race(&self) {
        self.state.lock().unwrap().create_behavior = CreateBehavior::RaceWithMatchingWorker;
    }

    pub fn simulate_unavailable_worker_after_create_race(&self) {
        self.state.lock().unwrap().create_behavior = CreateBehavior::AlreadyExistsWithoutWorker;
    }

    pub fn fail_next_worker_lookup_with_unknown(&self) {
        self.state.lock().unwrap().fail_next_worker_lookup = true;
    }

    pub fn malform_next_worker_lookup(&self) {
        self.state.lock().unwrap().malformed_next_worker_lookup = true;
    }

    async fn accepted_job_stream(
        &self,
        job_request: JobRequest,
        grpc_timeout: Option<String>,
        client_stream: bool,
    ) -> Result<Response<RpcStream<ResultOutputItem>>, Status> {
        let workflow_items = self.preparation_workflow_items(&job_request).await?;
        let (behavior, result_items, job_id, enqueue_gate, output_gate, trailing_error) = {
            let mut state = self.state.lock().unwrap();
            state.counts.job_enqueue_calls += 1;
            if client_stream {
                state.counts.enqueue_with_client_stream_calls += 1;
            } else {
                state.counts.enqueue_for_stream_calls += 1;
            }
            state.job_requests.push(job_request);
            state.grpc_timeout_headers.push(grpc_timeout);
            let behavior = std::mem::take(&mut state.enqueue_behavior);
            (
                behavior,
                workflow_items.unwrap_or_else(|| state.job_result_items.clone()),
                9_003 + state.counts.job_enqueue_calls as i64,
                state.enqueue_gate.take(),
                state.output_gate.take(),
                std::mem::take(&mut state.trailing_stream_error),
            )
        };
        if let Some(gate) = enqueue_gate {
            gate.entered.notify_one();
            gate.resume.notified().await;
        }
        if behavior == EnqueueBehavior::Unavailable {
            return Err(Status::unavailable("injected enqueue failure"));
        }

        let (output_tx, output_rx) = mpsc::channel(result_items.len().max(1));
        tokio::spawn(async move {
            if let Some(gate) = output_gate {
                gate.entered.notify_one();
                gate.resume.notified().await;
            }
            for item in result_items {
                if output_tx.send(Ok(item)).await.is_err() {
                    break;
                }
            }
            if trailing_error {
                let _ = output_tx
                    .send(Err(Status::unknown("injected error after output")))
                    .await;
            }
        });
        let output_stream: RpcStream<ResultOutputItem> = Box::pin(ReceiverStream::new(output_rx));
        let mut response = Response::new(output_stream);
        if behavior != EnqueueBehavior::MissingJobId {
            let id = if behavior == EnqueueBehavior::InvalidJobId {
                JobId { value: 0 }
            } else {
                JobId { value: job_id }
            };
            let encoded_id = id.encode_to_vec();
            let value = tonic::metadata::MetadataValue::from_bytes(&encoded_id);
            response
                .metadata_mut()
                .insert_bin(JOB_ID_HEADER_NAME, value);
        }
        Ok(response)
    }

    async fn preparation_workflow_items(
        &self,
        request: &JobRequest,
    ) -> Result<Option<Vec<ResultOutputItem>>, Status> {
        use jobworkerp_client::{
            jobworkerp::{
                data::{Trailer, result_output_item::Item},
                service::job_request,
            },
            proto::JobworkerpProto,
        };
        use serde_json::{Value, json};
        use tokio::io::AsyncWriteExt;
        let Some(runner) = self.preparation_workflow_runner.lock().unwrap().clone() else {
            return Ok(None);
        };
        let worker = {
            let state = self.state.lock().unwrap();
            state
                .workers
                .values()
                .find(|worker| match &request.worker {
                    Some(job_request::Worker::WorkerId(id)) => worker.id.as_ref() == Some(id),
                    Some(job_request::Worker::WorkerName(name)) => {
                        worker.data.as_ref().is_some_and(|data| &data.name == name)
                    }
                    None => false,
                })
                .and_then(|worker| worker.data.clone())
        };
        let Some(worker) = worker.filter(|data| {
            data.runner_id
                == Some(RunnerId {
                    value: SANDBOX_RUNNER_ID + 1,
                })
        }) else {
            return Ok(None);
        };
        let decode = |descriptor, bytes: &[u8]| -> Value {
            serde_json::to_value(prost_reflect::DynamicMessage::decode(descriptor, bytes).unwrap())
                .unwrap()
        };
        let settings = decode(
            JobworkerpProto::parse_runner_settings_schema_descriptor(&runner)
                .unwrap()
                .unwrap(),
            &worker.runner_settings,
        );
        let definition: Value =
            serde_json::from_str(settings["workflowData"].as_str().unwrap()).unwrap();
        if definition["do"][0].get("runAgent").is_some()
            || definition["do"][0].get("commitWorkspace").is_some()
            || definition["do"][0].get("validateWorkspace").is_some()
        {
            // A configured RPC result tests transport; it does not execute the workflow DSL.
            return Ok(Some(self.state.lock().unwrap().job_result_items.clone()));
        }
        let arguments = &definition["do"][0]["invokeExecutor"]["run"]["function"]["arguments"];
        let args = decode(
            JobworkerpProto::parse_job_args_schema_descriptor(&runner, Some("run"))
                .unwrap()
                .unwrap(),
            &request.args,
        );
        let input: Value = serde_json::from_str(args["input"].as_str().unwrap()).unwrap();
        let mut child = tokio::process::Command::new(arguments["command"].as_str().unwrap())
            .args(
                arguments["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|arg| arg.as_str().unwrap()),
            )
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input["executor_request_json"].as_str().unwrap().as_bytes())
            .await
            .unwrap();
        let output = child.wait_with_output().await.unwrap();
        let result = json!({"id":"fixture-preparation-workflow", "status":if output.status.success() { "Completed" } else { "Faulted" },
            "output":serde_json::to_string(&String::from_utf8(output.stdout).unwrap()).unwrap()});
        let bytes = JobworkerpProto::json_value_to_message(
            JobworkerpProto::parse_result_schema_descriptor(&runner, Some("run"))
                .unwrap()
                .unwrap(),
            &result,
            true,
            false,
        )
        .unwrap();
        Ok(Some(vec![
            ResultOutputItem {
                item: Some(Item::Data(bytes)),
            },
            ResultOutputItem {
                item: Some(Item::End(Trailer::default())),
            },
        ]))
    }
}

pub fn sandbox_runner_data() -> RunnerData {
    let mut schemas = HashMap::new();
    schemas.insert(
        "run".to_owned(),
        MethodSchema {
            args_proto: resolved_sandbox_schema(CLIENT_ARGS_PROTO),
            result_proto: RESULT_PROTO.to_owned(),
            description: Some("Execute a command in a local microsandbox VM".to_owned()),
            output_type: StreamingOutputType::Streaming as i32,
            require_client_stream: false,
            client_stream_data_proto: None,
        },
    );
    schemas.insert(
        "run_with_client".to_owned(),
        MethodSchema {
            args_proto: resolved_sandbox_schema(CLIENT_ARGS_PROTO),
            result_proto: RESULT_PROTO.to_owned(),
            description: Some(
                "Execute a command in a local microsandbox VM with client stdin".to_owned(),
            ),
            output_type: StreamingOutputType::Streaming as i32,
            require_client_stream: true,
            client_stream_data_proto: Some(String::new()),
        },
    );
    RunnerData {
        name: "SANDBOX".to_owned(),
        description: "Native SANDBOX test runner".to_owned(),
        runner_type: jobworkerp_client::jobworkerp::data::RunnerType::Sandbox as i32,
        runner_settings_proto: resolved_sandbox_schema(SETTINGS_PROTO),
        definition: "builtin:sandbox".to_owned(),
        method_proto_map: Some(MethodProtoMap { schemas }),
    }
}

pub struct TestServer {
    pub url: String,
    shutdown: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.join.abort();
    }
}

pub async fn start_server(fixture: RpcFixture) -> TestServer {
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
    let service_fixture = fixture.clone();
    let join = tokio::spawn(async move {
        Server::builder()
            .add_service(JobServiceServer::new(service_fixture.clone()))
            .add_service(WorkerServiceServer::new(service_fixture.clone()))
            .add_service(RunnerServiceServer::new(service_fixture))
            .serve_with_incoming_shutdown(ReceiverStream::new(incoming_rx), async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    TestServer {
        url: format!("http://{address}"),
        shutdown: Some(shutdown_tx),
        join,
    }
}

#[tonic::async_trait]
impl JobService for RpcFixture {
    type EnqueueForStreamStream = RpcStream<ResultOutputItem>;
    type EnqueueForResultStream = RpcStream<jobworkerp_client::jobworkerp::data::JobResult>;
    type EnqueueWithClientStreamStream = RpcStream<ResultOutputItem>;
    type FindListStream = RpcStream<Job>;
    type FindQueueListStream = RpcStream<JobAndStatus>;
    type FindListWithProcessingStatusStream = RpcStream<JobAndStatus>;

    async fn enqueue(
        &self,
        _request: Request<JobRequest>,
    ) -> Result<Response<jobworkerp_client::jobworkerp::service::CreateJobResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn enqueue_for_result(
        &self,
        _request: Request<JobRequest>,
    ) -> Result<Response<Self::EnqueueForResultStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn enqueue_for_stream(
        &self,
        request: Request<JobRequest>,
    ) -> Result<Response<Self::EnqueueForStreamStream>, Status> {
        let grpc_timeout = request
            .metadata()
            .get("grpc-timeout")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        self.accepted_job_stream(request.into_inner(), grpc_timeout, false)
            .await
    }

    async fn enqueue_with_client_stream(
        &self,
        request: Request<tonic::Streaming<ClientStreamRequest>>,
    ) -> Result<Response<Self::EnqueueWithClientStreamStream>, Status> {
        let grpc_timeout = request
            .metadata()
            .get("grpc-timeout")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut requests = request.into_inner();
        let initial = requests
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("missing initial JobRequest"))?;
        let Some(client_stream_request::Request::JobRequest(job_request)) = initial.request else {
            return Err(Status::invalid_argument(
                "first client stream frame must contain JobRequest",
            ));
        };

        let response = self
            .accepted_job_stream(job_request, grpc_timeout, true)
            .await?;

        let fixture = self.clone();
        tokio::spawn(async move {
            loop {
                match requests.message().await {
                    Ok(Some(ClientStreamRequest {
                        request: Some(client_stream_request::Request::FeedData(frame)),
                    })) => {
                        let is_final = frame.is_final;
                        fixture.state.lock().unwrap().feed_frames.push(frame);
                        if is_final {
                            fixture.input_notify.notify_one();
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        });

        Ok(response)
    }

    async fn delete(&self, request: Request<JobId>) -> Result<Response<SuccessResponse>, Status> {
        let mut state = self.state.lock().unwrap();
        state.counts.job_delete_calls += 1;
        state.cancelled_job_ids.push(request.into_inner());
        Err(Status::unimplemented(
            "native session must not delete its job",
        ))
    }

    async fn find(
        &self,
        _request: Request<JobId>,
    ) -> Result<Response<jobworkerp_client::jobworkerp::service::OptionalJobResponse>, Status> {
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
        _request: Request<jobworkerp_client::jobworkerp::service::FindQueueListRequest>,
    ) -> Result<Response<Self::FindQueueListStream>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn find_list_with_processing_status(
        &self,
        _request: Request<
            jobworkerp_client::jobworkerp::service::FindListWithProcessingStatusRequest,
        >,
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
impl WorkerService for RpcFixture {
    type FindListStream = RpcStream<Worker>;

    async fn create(
        &self,
        request: Request<WorkerData>,
    ) -> Result<Response<CreateWorkerResponse>, Status> {
        let data = request.into_inner();
        let mut state = self.state.lock().unwrap();
        state.counts.create_calls += 1;
        if state.workers.contains_key(&data.name) {
            return Err(Status::already_exists("worker name already exists"));
        }
        let behavior = std::mem::take(&mut state.create_behavior);
        if behavior == CreateBehavior::AlreadyExistsWithoutWorker {
            return Err(Status::already_exists(
                "another creator won but the worker is not visible yet",
            ));
        }
        let id = WorkerId {
            value: FIRST_WORKER_ID + state.created_workers.len() as i64,
        };
        state.created_workers.push(data.clone());
        state.workers.insert(
            data.name.clone(),
            Worker {
                id: Some(id),
                data: Some(data),
            },
        );
        if behavior == CreateBehavior::RaceWithMatchingWorker {
            return Err(Status::already_exists("worker was created concurrently"));
        }
        Ok(Response::new(CreateWorkerResponse { id: Some(id) }))
    }

    async fn upsert_by_name(
        &self,
        request: Request<WorkerData>,
    ) -> Result<Response<CreateWorkerResponse>, Status> {
        let mut state = self.state.lock().unwrap();
        state.counts.upsert_calls += 1;
        if state.allow_workflow_upsert {
            let data = request.into_inner();
            let id = state
                .workers
                .get(&data.name)
                .and_then(|worker| worker.id)
                .unwrap_or(WorkerId {
                    value: FIRST_WORKER_ID + state.created_workers.len() as i64,
                });
            state.created_workers.push(data.clone());
            state.workers.insert(
                data.name.clone(),
                Worker {
                    id: Some(id),
                    data: Some(data),
                },
            );
            return Ok(Response::new(CreateWorkerResponse { id: Some(id) }));
        }
        Err(Status::unimplemented(
            "native worker must never upsert by name",
        ))
    }

    async fn update(&self, _request: Request<Worker>) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn delete(
        &self,
        _request: Request<WorkerId>,
    ) -> Result<Response<SuccessResponse>, Status> {
        self.state.lock().unwrap().counts.worker_delete_calls += 1;
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn find(
        &self,
        request: Request<WorkerId>,
    ) -> Result<Response<OptionalWorkerResponse>, Status> {
        let id = request.into_inner().value;
        let data = self
            .state
            .lock()
            .unwrap()
            .workers
            .values()
            .find(|worker| {
                worker
                    .id
                    .as_ref()
                    .is_some_and(|worker_id| worker_id.value == id)
            })
            .cloned();
        Ok(Response::new(OptionalWorkerResponse { data }))
    }

    async fn find_by_name(
        &self,
        request: Request<WorkerNameRequest>,
    ) -> Result<Response<OptionalWorkerResponse>, Status> {
        let gate = {
            let mut state = self.state.lock().unwrap();
            if state
                .worker_lookup_gate
                .as_ref()
                .is_some_and(|(target, _)| *target == state.counts.worker_lookups + 1)
            {
                state.worker_lookup_gate.take().map(|(_, gate)| gate)
            } else {
                None
            }
        };
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.resume.notified().await;
        }
        let name = request.into_inner().name;
        let mut state = self.state.lock().unwrap();
        state.counts.worker_lookups += 1;
        if state.fail_next_worker_lookup {
            state.fail_next_worker_lookup = false;
            return Err(Status::unknown("injected worker lookup failure"));
        }
        if state.malformed_next_worker_lookup {
            state.malformed_next_worker_lookup = false;
            return Ok(Response::new(OptionalWorkerResponse {
                data: Some(Worker {
                    id: Some(WorkerId {
                        value: FIRST_WORKER_ID,
                    }),
                    data: None,
                }),
            }));
        }
        let mut data = state.workers.get(&name).cloned();
        if let (Some(worker), Some(replacement_id)) = (&mut data, state.replace_worker_id.take()) {
            worker.id = Some(replacement_id);
        }
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
    ) -> Result<Response<jobworkerp_client::jobworkerp::service::FindChannelListResponse>, Status>
    {
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn release_static_worker(
        &self,
        _request: Request<jobworkerp_client::jobworkerp::service::ReleaseStaticWorkerRequest>,
    ) -> Result<Response<SuccessResponse>, Status> {
        self.state.lock().unwrap().counts.release_static_calls += 1;
        Err(Status::unimplemented("unused fixture RPC"))
    }

    async fn load(
        &self,
        _request: Request<jobworkerp_client::jobworkerp::service::LoadWorkerRequest>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused fixture RPC"))
    }
}

#[tonic::async_trait]
impl RunnerService for RpcFixture {
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
        request: Request<RunnerId>,
    ) -> Result<Response<OptionalRunnerResponse>, Status> {
        let mut state = self.state.lock().unwrap();
        if state.fail_next_runner_lookup {
            state.fail_next_runner_lookup = false;
            return Err(Status::unknown("injected runner lookup failure"));
        }
        if state.malformed_next_runner_lookup {
            state.malformed_next_runner_lookup = false;
            return Ok(Response::new(OptionalRunnerResponse {
                data: Some(Runner {
                    id: None,
                    data: None,
                }),
            }));
        }
        let runner_data = self.runner_data.lock().unwrap().clone();
        let id = request.into_inner().value;
        let runner = if id == SANDBOX_RUNNER_ID {
            Some(Runner {
                id: Some(RunnerId {
                    value: SANDBOX_RUNNER_ID,
                }),
                data: Some(runner_data),
            })
        } else if id == SANDBOX_RUNNER_ID + 1 {
            if let Some((name, remove)) = state.change_after_workflow_runner_lookup.take() {
                if remove {
                    state.workers.remove(&name);
                } else {
                    state
                        .workers
                        .get_mut(&name)
                        .unwrap()
                        .id
                        .as_mut()
                        .unwrap()
                        .value += 100;
                }
            }
            self.preparation_workflow_runner
                .lock()
                .unwrap()
                .clone()
                .map(|data| Runner {
                    id: Some(RunnerId { value: id }),
                    data: Some(data),
                })
        } else {
            None
        };
        Ok(Response::new(OptionalRunnerResponse { data: runner }))
    }

    async fn find_by_name(
        &self,
        request: Request<RunnerNameRequest>,
    ) -> Result<Response<OptionalRunnerResponse>, Status> {
        self.state.lock().unwrap().counts.runner_lookups += 1;
        let runner_data = self.runner_data.lock().unwrap().clone();
        let name = request.into_inner().name;
        let data = if name == runner_data.name {
            Some(Runner {
                id: Some(RunnerId {
                    value: SANDBOX_RUNNER_ID,
                }),
                data: Some(runner_data),
            })
        } else if name == "WORKFLOW" {
            self.preparation_workflow_runner
                .lock()
                .unwrap()
                .clone()
                .map(|data| Runner {
                    id: Some(RunnerId {
                        value: SANDBOX_RUNNER_ID + 1,
                    }),
                    data: Some(data),
                })
        } else {
            None
        };
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
