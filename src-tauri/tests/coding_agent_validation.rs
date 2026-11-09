use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use coding_agent_executor::{
    CapabilityDocument, CaptureGuarantee, EXECUTOR_VERSION, PROTOCOL_VERSION, RepositoryConfig,
    RepositoryId, ResolvedArtifact, SealedArtifactReceipt, TrustedConfig,
};
use futures::Stream;
use jobworkerp_client::jobworkerp::{
    data::{
        Empty, JobId, MethodProtoMap, MethodSchema, ResponseType, ResultOutputItem, Runner,
        RunnerData, RunnerId, StreamingOutputType, Trailer, Worker, WorkerData, WorkerId,
        result_output_item::Item,
    },
    service::{
        self, ClientStreamRequest, CountCondition, CountResponse, CountRunnerRequest,
        CountWorkerRequest, CreateJobResponse, CreateRunnerRequest, CreateRunnerResponse,
        CreateWorkerResponse, FindListRequest, FindListWithProcessingStatusRequest,
        FindQueueListRequest, FindRunnerListRequest, FindWorkerListRequest, JobAndStatus,
        JobRequest, LoadWorkerRequest, OptionalJobResponse, OptionalRunnerResponse,
        OptionalWorkerResponse, ReleaseStaticWorkerRequest, RunnerNameRequest, SuccessResponse,
        WorkerNameRequest, job_request,
        job_service_server::{JobService, JobServiceServer},
        runner_service_server::{RunnerService, RunnerServiceServer},
        worker_service_server::{WorkerService, WorkerServiceServer},
    },
};
use lookback_tauri_lib::{
    coding_agent::{
        self as coding_agent, NativeExecutorClient,
        assessment::{AssessmentEngine, AssessmentStore, ProviderSelection},
        coordinator::ClaimedAssessmentConfig,
        judge_provider,
        store::RunStore,
    },
    coding_agent::{
        AgentProfileSnapshot, AgentRunSnapshot, AllowedOperation, ApprovedValidationPlan,
        CancellationObservation, ChildWritersStatus, CodingAgentCoordinator, ExecutionAdapter,
        ExecutionEvent, ExecutionIdentity, ExecutionInvocationKind, ExecutionMode, ExecutionResult,
        ForgeKind, HostFinalizationObservation, HostResourceLimits, ImmutableExecutionIntent,
        InteractiveFinishObservation, MAX_VALIDATION_OUTPUT_BYTES, OperationReconciliation,
        PreparationOutcome, ProcessExitObservation, PromptDelivery, PublicationAdapter,
        PublicationOutcome, PublicationPlan, PublicationRequest, PublicationSupport,
        RepositoryIdentity, RepositoryProvenance, RepositorySelection, ResolvedRepository,
        RunApproval, RunId, RunPurpose, RunState, RuntimeObservation, SandboxResources,
        ShellProcessObservation, StartReceipt, TerminalControl, ValidationEvidence,
        ValidationEvidenceStore, ValidationOutcome, ValidationOutputContext, ValidationOutputStore,
        ValidationRecord, ValidationState, ValidationTargetSnapshot, WorkflowDefinitionSnapshot,
        WorkspaceFreezeStatus, WorkspaceReceipt, WorkspaceRequest,
        execution::{
            ApprovedAssessmentContextSource, ExecutionEventStream,
            NativeExecutorFinalizationSources, NativeSealedArtifactLookup,
            NativeSealedArtifactRecord, SealedArtifactBinding, TypedFinalizationEvidence,
            ValidationPlanRuntime, ValidationPlanRuntimeSource, VerifiedFinalization,
            VerifiedFinalizationSources,
        },
        find_reusable_validation,
        model::{
            ApprovalAssessmentCommand, ApprovalAssessmentContract, ApprovalCriterionPolicy,
            ApprovalValidationDraft, AssessmentInputScope, CriterionEvaluationMethod,
            StoredAgentRunSnapshot, ValidationPermission,
        },
        record_validation_not_run, run_approved_validation,
    },
    data::DataPaths,
    grpc::proto::jobworkerp::runner::sandbox::{
        SandboxExecExit, SandboxExecOutput, SandboxExecResult,
        sandbox_exec_result::Result as SandboxResult,
    },
    jobworkerp::JobworkerpHandle,
};
use prost::Message;
use sha2::Digest;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, transport::Server};

#[path = "fixtures/coding_agent_claimed_rpc.rs"]
mod claimed_rpc;

const JOB_ID: i64 = 843;
const RUNNER_ID: i64 = 19;
const WORKER_ID: i64 = 71;
const EXPECTED_SANDBOX_ID: &str = "sandbox-test";

#[derive(Clone, PartialEq, Message)]
struct CapturedSandboxArgs {
    #[prost(string, tag = "1")]
    command: String,
    #[prost(string, repeated, tag = "2")]
    args: Vec<String>,
    #[prost(string, optional, tag = "3")]
    working_directory: Option<String>,
    #[prost(btree_map = "string, string", tag = "4")]
    environment: BTreeMap<String, String>,
    #[prost(uint64, optional, tag = "6")]
    timeout_ms: Option<u64>,
    #[prost(bool, tag = "7")]
    treat_nonzero_as_error: bool,
}

#[derive(Clone, Copy)]
enum ResultBehavior {
    Success,
    NonzeroWithPassingLookingOutput,
    MissingEnd,
    MissingJobId,
    InvalidJobId,
    DuplicateExit,
    WrongSandbox,
    StreamError,
    OversizedOutput,
    UnknownOutputStream,
    EmptySandboxId,
}

#[derive(Clone)]
struct Fixture {
    behavior: ResultBehavior,
    requests: Arc<std::sync::Mutex<Vec<JobRequest>>>,
}

impl Fixture {
    fn items(&self) -> Vec<ResultOutputItem> {
        let output = |stream: &str, data: Vec<u8>| ResultOutputItem {
            item: Some(Item::Data(
                SandboxExecResult {
                    result: Some(SandboxResult::Output(SandboxExecOutput {
                        stream: stream.to_owned(),
                        data,
                    })),
                }
                .encode_to_vec(),
            )),
        };
        let (code, sandbox_id) = match self.behavior {
            ResultBehavior::NonzeroWithPassingLookingOutput => (1, EXPECTED_SANDBOX_ID),
            ResultBehavior::WrongSandbox => (0, "different-sandbox"),
            ResultBehavior::EmptySandboxId => (0, ""),
            _ => (0, EXPECTED_SANDBOX_ID),
        };
        let stdout = if matches!(
            self.behavior,
            ResultBehavior::NonzeroWithPassingLookingOutput
        ) {
            b"all tests passed\n".to_vec()
        } else {
            b"test output\n".to_vec()
        };
        let mut items = if matches!(self.behavior, ResultBehavior::OversizedOutput) {
            let chunk_size = 1024 * 1024;
            let mut items = Vec::with_capacity(MAX_VALIDATION_OUTPUT_BYTES / chunk_size + 2);
            for _ in 0..(MAX_VALIDATION_OUTPUT_BYTES / chunk_size) {
                items.push(output("stdout", vec![b'x'; chunk_size]));
            }
            let remainder = MAX_VALIDATION_OUTPUT_BYTES % chunk_size;
            items.push(output("stdout", vec![b'x'; remainder + 1]));
            items
        } else {
            let stream = if matches!(self.behavior, ResultBehavior::UnknownOutputStream) {
                "debug"
            } else {
                "stdout"
            };
            vec![output(stream, stdout)]
        };
        if !matches!(self.behavior, ResultBehavior::OversizedOutput) {
            items.push(output("stderr", b"warning\n".to_vec()));
        }
        items.push(ResultOutputItem {
            item: Some(Item::Data(
                SandboxExecResult {
                    result: Some(SandboxResult::Exit(SandboxExecExit {
                        exit_code: code,
                        execution_time_ms: 17,
                        sandbox_id: sandbox_id.to_owned(),
                    })),
                }
                .encode_to_vec(),
            )),
        });
        if matches!(self.behavior, ResultBehavior::DuplicateExit) {
            items.push(ResultOutputItem {
                item: Some(Item::Data(
                    SandboxExecResult {
                        result: Some(SandboxResult::Exit(SandboxExecExit {
                            exit_code: code,
                            execution_time_ms: 18,
                            sandbox_id: sandbox_id.to_owned(),
                        })),
                    }
                    .encode_to_vec(),
                )),
            });
        }
        if !matches!(self.behavior, ResultBehavior::MissingEnd) {
            let metadata = if matches!(self.behavior, ResultBehavior::StreamError) {
                HashMap::from([("stream_error".to_owned(), "execution failed".to_owned())])
            } else {
                HashMap::new()
            };
            items.push(ResultOutputItem {
                item: Some(Item::End(Trailer {
                    metadata,
                    ..Default::default()
                })),
            });
        }
        items
    }

    fn worker_data(&self) -> WorkerData {
        WorkerData {
            name: "sandbox-worker".to_owned(),
            runner_id: Some(RunnerId { value: RUNNER_ID }),
            response_type: ResponseType::Direct as i32,
            ..Default::default()
        }
    }

    fn runner_data(&self) -> RunnerData {
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
        let args_proto = format!("{args}\n{common}");
        let result_proto =
            include_str!("fixtures/jobworkerp-proto/sandbox_result.proto").to_owned();
        let schema = MethodSchema {
            args_proto,
            result_proto,
            description: None,
            output_type: StreamingOutputType::Streaming as i32,
            require_client_stream: false,
            client_stream_data_proto: None,
        };
        RunnerData {
            name: "SANDBOX".to_owned(),
            method_proto_map: Some(MethodProtoMap {
                schemas: HashMap::from([("run".to_owned(), schema)]),
            }),
            ..Default::default()
        }
    }
}

type RpcStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

#[derive(Clone)]
struct MockJobService(Fixture);

#[tonic::async_trait]
impl JobService for MockJobService {
    type EnqueueForStreamStream = RpcStream<ResultOutputItem>;
    type EnqueueForResultStream = RpcStream<jobworkerp_client::jobworkerp::data::JobResult>;
    type EnqueueWithClientStreamStream = RpcStream<ResultOutputItem>;
    type FindListStream = RpcStream<jobworkerp_client::jobworkerp::data::Job>;
    type FindQueueListStream = RpcStream<JobAndStatus>;
    type FindListWithProcessingStatusStream = RpcStream<JobAndStatus>;

    async fn enqueue(
        &self,
        _request: Request<JobRequest>,
    ) -> Result<Response<CreateJobResponse>, Status> {
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
        self.0.requests.lock().unwrap().push(request.into_inner());
        let response_items = self.0.items().into_iter().map(Ok).collect::<Vec<_>>();
        let output =
            Box::pin(futures::stream::iter(response_items)) as Self::EnqueueForStreamStream;
        let mut response = Response::new(output);
        if !matches!(self.0.behavior, ResultBehavior::MissingJobId) {
            let job_id = if matches!(self.0.behavior, ResultBehavior::InvalidJobId) {
                0
            } else {
                JOB_ID
            };
            response.metadata_mut().insert_bin(
                "x-job-id-bin",
                tonic::metadata::MetadataValue::from_bytes(
                    &JobId { value: job_id }.encode_to_vec(),
                ),
            );
        }
        Ok(response)
    }

    async fn enqueue_with_client_stream(
        &self,
        _request: Request<tonic::Streaming<ClientStreamRequest>>,
    ) -> Result<Response<Self::EnqueueWithClientStreamStream>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn delete(&self, _request: Request<JobId>) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn find(
        &self,
        _request: Request<JobId>,
    ) -> Result<Response<OptionalJobResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn find_list(
        &self,
        _request: Request<FindListRequest>,
    ) -> Result<Response<Self::FindListStream>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn find_queue_list(
        &self,
        _request: Request<FindQueueListRequest>,
    ) -> Result<Response<Self::FindQueueListStream>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn find_list_with_processing_status(
        &self,
        _request: Request<FindListWithProcessingStatusRequest>,
    ) -> Result<Response<Self::FindListWithProcessingStatusStream>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn count(
        &self,
        _request: Request<CountCondition>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused"))
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
        Err(Status::unimplemented("unused"))
    }

    async fn upsert_by_name(
        &self,
        _request: Request<WorkerData>,
    ) -> Result<Response<CreateWorkerResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn update(&self, _request: Request<Worker>) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn delete(
        &self,
        _request: Request<WorkerId>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn find(
        &self,
        _request: Request<WorkerId>,
    ) -> Result<Response<OptionalWorkerResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn find_by_name(
        &self,
        _request: Request<WorkerNameRequest>,
    ) -> Result<Response<OptionalWorkerResponse>, Status> {
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
        Err(Status::unimplemented("unused"))
    }

    async fn count(
        &self,
        _request: Request<CountCondition>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn count_by(
        &self,
        _request: Request<CountWorkerRequest>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn find_channel_list(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<service::FindChannelListResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn release_static_worker(
        &self,
        _request: Request<ReleaseStaticWorkerRequest>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn load(
        &self,
        _request: Request<LoadWorkerRequest>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused"))
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
        Err(Status::unimplemented("unused"))
    }

    async fn delete(
        &self,
        _request: Request<RunnerId>,
    ) -> Result<Response<SuccessResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn find(
        &self,
        _request: Request<RunnerId>,
    ) -> Result<Response<OptionalRunnerResponse>, Status> {
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
        Err(Status::unimplemented("unused"))
    }

    async fn find_list(
        &self,
        _request: Request<FindListRequest>,
    ) -> Result<Response<Self::FindListStream>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn count(
        &self,
        _request: Request<CountCondition>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn find_list_by(
        &self,
        _request: Request<FindRunnerListRequest>,
    ) -> Result<Response<Self::FindListByStream>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn count_by(
        &self,
        _request: Request<CountRunnerRequest>,
    ) -> Result<Response<CountResponse>, Status> {
        Err(Status::unimplemented("unused"))
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
    let join = tokio::spawn(async move {
        Server::builder()
            .add_service(JobServiceServer::new(MockJobService(fixture.clone())))
            .add_service(WorkerServiceServer::new(MockWorkerService(fixture.clone())))
            .add_service(RunnerServiceServer::new(MockRunnerService(fixture)))
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

#[derive(Default)]
struct MemoryOutputStore {
    output: Option<(Vec<u8>, Vec<u8>)>,
    output_reference: Option<String>,
    binding: Option<(String, String, String, String)>,
    records: Vec<ValidationRecord>,
    fail_persistence: bool,
    invalid_reference: bool,
}

impl ValidationOutputStore for MemoryOutputStore {
    fn persist_validation_output(
        &mut self,
        context: &ValidationOutputContext<'_>,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<String, lookback_tauri_lib::coding_agent::ValidationOutputStoreError> {
        if self.fail_persistence {
            return Err(lookback_tauri_lib::coding_agent::ValidationOutputStoreError);
        }
        self.binding = Some((
            context.run_id().to_owned(),
            context.attempt_id().to_owned(),
            context.validation_id().to_owned(),
            context.job_id().to_owned(),
        ));
        self.output = Some((stdout.to_vec(), stderr.to_vec()));
        let reference = context.output_reference();
        self.output_reference = Some(reference.clone());
        if self.invalid_reference {
            return Ok("../../agent-log.txt".to_owned());
        }
        Ok(reference)
    }

    fn persist_validation_record(
        &mut self,
        record: &ValidationRecord,
    ) -> Result<(), lookback_tauri_lib::coding_agent::ValidationOutputStoreError> {
        self.records.push(record.clone());
        Ok(())
    }
}

fn plan() -> ApprovedValidationPlan {
    let target =
        ValidationTargetSnapshot::new("a".repeat(40), "b".repeat(40), "c".repeat(64)).unwrap();
    plan_with(
        "run-1",
        "attempt-1",
        "check-1",
        true,
        vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "--workspace".to_owned(),
        ],
        target,
    )
}

fn plan_with(
    run_id: &str,
    attempt_id: &str,
    validation_id: &str,
    required: bool,
    argv: Vec<String>,
    target: ValidationTargetSnapshot,
) -> ApprovedValidationPlan {
    ApprovedValidationPlan::new(
        run_id,
        attempt_id,
        validation_id,
        required,
        "sandbox-worker",
        EXPECTED_SANDBOX_ID,
        argv,
        "/workspace/repository",
        BTreeMap::from([("RUST_BACKTRACE".to_owned(), "0".to_owned())]),
        "d".repeat(64),
        Some(60_000),
        target,
    )
    .unwrap()
}

fn plan_without_expected_sandbox() -> ApprovedValidationPlan {
    ApprovedValidationPlan::new_for_worker(
        "run-1",
        "attempt-1",
        "check-1",
        true,
        "sandbox-worker",
        vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "--workspace".to_owned(),
        ],
        "/workspace/repository",
        BTreeMap::from([("RUST_BACKTRACE".to_owned(), "0".to_owned())]),
        "d".repeat(64),
        Some(60_000),
        ValidationTargetSnapshot::new("a".repeat(40), "b".repeat(40), "c".repeat(64)).unwrap(),
    )
    .unwrap()
}

fn fixture(behavior: ResultBehavior) -> Fixture {
    Fixture {
        behavior,
        requests: Arc::new(std::sync::Mutex::new(Vec::new())),
    }
}

#[tokio::test]
async fn only_a_clean_real_sandbox_job_with_persisted_output_issues_a_reusable_receipt() {
    let fixture = fixture(ResultBehavior::Success);
    let server = start_mock_server(fixture.clone()).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let approved = plan();
    let mut output_store = MemoryOutputStore::default();

    let record = run_approved_validation(&handle, &approved, &mut output_store)
        .await
        .unwrap();

    assert_eq!(record.outcome(), ValidationOutcome::Passed);
    let receipt = record.receipt().expect("host should issue a receipt");
    assert_eq!(receipt.schema_version(), 1);
    assert_eq!(receipt.run_id(), "run-1");
    assert_eq!(receipt.attempt_id(), "attempt-1");
    assert_eq!(receipt.validation_id(), "check-1");
    assert!(receipt.required());
    assert_eq!(receipt.child_job_id(), JOB_ID.to_string());
    assert_eq!(receipt.sandbox_id(), EXPECTED_SANDBOX_ID);
    assert_eq!(receipt.exit_code(), 0);
    assert_eq!(
        receipt.target_full_commit_id(),
        approved.target().full_commit_id()
    );
    assert_eq!(
        receipt.target_full_tree_id(),
        approved.target().full_tree_id()
    );
    assert_eq!(
        receipt.target_untracked_manifest_digest(),
        approved.target().untracked_manifest_digest()
    );
    assert_eq!(
        receipt.permission_profile_digest(),
        approved.permission_profile_digest()
    );
    assert!(receipt.end_observed());
    assert!(!receipt.stream_error_observed());
    assert_eq!(receipt.target_snapshot_digest(), approved.target().digest());
    assert_eq!(
        receipt.command_definition_digest(),
        approved.command_definition_digest()
    );
    assert_eq!(
        Some(receipt.output().reference().to_owned()),
        output_store.output_reference
    );
    assert_eq!(receipt.output().stdout_bytes(), b"test output\n".len());
    assert_eq!(
        receipt.output().stdout_sha256(),
        format!("{:x}", sha2::Sha256::digest(b"test output\n"))
    );
    assert_eq!(receipt.output().stderr_bytes(), b"warning\n".len());
    assert_eq!(
        receipt.output().stderr_sha256(),
        format!("{:x}", sha2::Sha256::digest(b"warning\n"))
    );
    assert!(receipt.output().capture_complete());
    let receipts = std::slice::from_ref(receipt);
    assert!(find_reusable_validation(&approved, receipts).is_some());
    assert!(
        find_reusable_validation(
            &plan_with(
                "run-1",
                "attempt-1",
                "check-1",
                true,
                vec!["cargo".to_owned(), "test".to_owned(), "--all".to_owned()],
                approved.target().clone(),
            ),
            receipts,
        )
        .is_none()
    );
    assert!(
        find_reusable_validation(
            &plan_with(
                "run-1",
                "attempt-2",
                "check-1",
                true,
                approved.argv().to_vec(),
                approved.target().clone(),
            ),
            receipts,
        )
        .is_none()
    );
    assert!(
        find_reusable_validation(
            &plan_with(
                "run-1",
                "attempt-1",
                "check-2",
                true,
                approved.argv().to_vec(),
                approved.target().clone(),
            ),
            receipts,
        )
        .is_none()
    );
    assert!(
        find_reusable_validation(
            &plan_with(
                "run-1",
                "attempt-1",
                "check-1",
                true,
                approved.argv().to_vec(),
                ValidationTargetSnapshot::new("e".repeat(40), "f".repeat(40), "0".repeat(64),)
                    .unwrap(),
            ),
            receipts,
        )
        .is_none()
    );
    let changed_bound_plan = |run_id: &str, environment_value: &str, permission_digest: &str| {
        ApprovedValidationPlan::new(
            run_id,
            "attempt-1",
            "check-1",
            true,
            "sandbox-worker",
            EXPECTED_SANDBOX_ID,
            approved.argv().to_vec(),
            "/workspace/repository",
            BTreeMap::from([("RUST_BACKTRACE".to_owned(), environment_value.to_owned())]),
            permission_digest.to_owned(),
            Some(60_000),
            approved.target().clone(),
        )
        .unwrap()
    };
    for changed in [
        changed_bound_plan("run-2", "0", &"d".repeat(64)),
        changed_bound_plan("run-1", "1", &"d".repeat(64)),
        changed_bound_plan("run-1", "0", &"e".repeat(64)),
    ] {
        assert!(find_reusable_validation(&changed, receipts).is_none());
    }
    assert_eq!(
        output_store.output,
        Some((b"test output\n".to_vec(), b"warning\n".to_vec()))
    );
    assert_eq!(
        output_store.binding,
        Some((
            "run-1".to_owned(),
            "attempt-1".to_owned(),
            "check-1".to_owned(),
            JOB_ID.to_string(),
        ))
    );
    assert_eq!(output_store.records.len(), 1);
    assert_eq!(
        output_store.records[0].receipt().unwrap().receipt_id(),
        receipt.receipt_id()
    );

    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.using.as_deref(), Some("run"));
    let worker = request.worker.as_ref().expect("worker selected");
    assert!(matches!(worker, job_request::Worker::WorkerId(id) if id.value == WORKER_ID));
    let args = CapturedSandboxArgs::decode(request.args.as_slice()).unwrap();
    assert_eq!(args.command, "cargo");
    assert_eq!(args.args, ["test", "--workspace"]);
    assert_eq!(
        args.working_directory.as_deref(),
        Some("/workspace/repository")
    );
    assert_eq!(
        args.environment,
        BTreeMap::from([("RUST_BACKTRACE".to_owned(), "0".to_owned())])
    );
    assert_eq!(args.timeout_ms, Some(60_000));
    assert!(!args.treat_nonzero_as_error);
}

#[tokio::test]
async fn sandbox_identity_is_observed_when_the_host_cannot_know_it_before_dispatch() {
    let fixture = fixture(ResultBehavior::Success);
    let server = start_mock_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let approved = plan_without_expected_sandbox();
    let mut output_store = MemoryOutputStore::default();

    let record = run_approved_validation(&handle, &approved, &mut output_store)
        .await
        .unwrap();

    assert_eq!(record.outcome(), ValidationOutcome::Passed);
    let receipt = record.receipt().unwrap();
    assert_eq!(receipt.expected_sandbox_id(), None);
    assert_eq!(receipt.sandbox_id(), EXPECTED_SANDBOX_ID);
    let known_id_plan = plan();
    assert_eq!(
        approved.command_definition_digest(),
        known_id_plan.command_definition_digest()
    );
    assert_ne!(approved.binding_digest(), known_id_plan.binding_digest());
    assert!(find_reusable_validation(&known_id_plan, std::slice::from_ref(receipt)).is_none());
}

#[tokio::test]
async fn successful_exit_without_durable_output_is_unknown_not_passed() {
    let fixture = fixture(ResultBehavior::Success);
    let server = start_mock_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let approved = plan();
    let mut output_store = MemoryOutputStore {
        fail_persistence: true,
        ..Default::default()
    };

    let record = run_approved_validation(&handle, &approved, &mut output_store)
        .await
        .unwrap();

    assert_eq!(record.outcome(), ValidationOutcome::Unknown);
    assert_eq!(record.exit_code(), Some(0));
    assert!(record.end_observed());
    assert!(record.receipt().is_none());
    assert_eq!(output_store.records.len(), 1);
    assert_eq!(
        output_store.records[0].outcome(),
        ValidationOutcome::Unknown
    );
}

#[tokio::test]
async fn invalid_store_reference_cannot_become_a_receipt() {
    let fixture = fixture(ResultBehavior::Success);
    let server = start_mock_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let approved = plan();
    let mut output_store = MemoryOutputStore {
        invalid_reference: true,
        ..Default::default()
    };

    let record = run_approved_validation(&handle, &approved, &mut output_store)
        .await
        .unwrap();

    assert_eq!(record.outcome(), ValidationOutcome::Unknown);
    assert!(record.receipt().is_none());
    assert_eq!(
        output_store.records[0].outcome(),
        ValidationOutcome::Unknown
    );
}

#[tokio::test]
#[cfg(unix)]
async fn native_store_reopens_persisted_output_record_and_receipt() {
    use std::fs;
    use tempfile::TempDir;

    fn private_dir(temp: &TempDir, name: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = temp.path().join(name);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    let temp = TempDir::new().unwrap();
    let root = private_dir(&temp, "native-store");
    let guest_root = private_dir(&temp, "guest-workspace");
    let fixture = fixture(ResultBehavior::Success);
    let server = start_mock_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let approved = plan_without_expected_sandbox();
    let mut store =
        ValidationEvidenceStore::open(&root, std::slice::from_ref(&guest_root)).unwrap();

    let record = run_approved_validation(&handle, &approved, &mut store)
        .await
        .unwrap();
    let original_receipt = record.receipt().unwrap().receipt_id().to_owned();
    assert_eq!(record.outcome(), ValidationOutcome::Passed);
    assert_eq!(record.receipt().unwrap().expected_sandbox_id(), None);
    store.persist_validation_record(&record).unwrap();
    drop(store);

    let reopened = ValidationEvidenceStore::open(&root, &[guest_root]).unwrap();
    let records = reopened.load_validation_records(&approved).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].receipt().unwrap().receipt_id(), original_receipt);
    assert_eq!(
        reopened
            .find_reusable_validation(&approved)
            .unwrap()
            .unwrap()
            .receipt_id(),
        original_receipt
    );
    let changed_target_plan = plan_with(
        "run-1",
        "attempt-1",
        "check-1",
        true,
        approved.argv().to_vec(),
        ValidationTargetSnapshot::new("e".repeat(40), "f".repeat(40), "0".repeat(64)).unwrap(),
    );
    assert!(
        reopened
            .find_reusable_validation(&changed_target_plan)
            .unwrap()
            .is_none()
    );

    let records_scope = fs::read_dir(root.join("records"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let record_dir = fs::read_dir(records_scope)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let receipt_path = record_dir.join("receipt.json");
    let receipt_bytes = fs::read(&receipt_path).unwrap();
    let receipt_text = String::from_utf8(receipt_bytes.clone()).unwrap();
    fs::write(
        &receipt_path,
        receipt_text.replacen('{', "{\"schema_version\":1,", 1),
    )
    .unwrap();
    assert!(reopened.find_reusable_validation(&approved).is_err());

    let mut unknown_version: serde_json::Value = serde_json::from_slice(&receipt_bytes).unwrap();
    unknown_version["schema_version"] = serde_json::json!(99);
    fs::write(&receipt_path, serde_json::to_vec(&unknown_version).unwrap()).unwrap();
    assert!(reopened.find_reusable_validation(&approved).is_err());

    fs::write(&receipt_path, receipt_bytes).unwrap();
    fs::remove_file(record_dir.join("receipt.json")).unwrap();
    assert!(reopened.find_reusable_validation(&approved).is_err());
}

#[tokio::test]
#[cfg(unix)]
async fn native_store_reopens_actual_failed_exit_without_reusing_it() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let root = temp.path().join("native-store");
    let guest_root = temp.path().join("guest-workspace");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&guest_root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&guest_root, fs::Permissions::from_mode(0o700)).unwrap();
    let fixture = fixture(ResultBehavior::NonzeroWithPassingLookingOutput);
    let server = start_mock_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let approved = plan();
    let mut store =
        ValidationEvidenceStore::open(&root, std::slice::from_ref(&guest_root)).unwrap();

    let record = run_approved_validation(&handle, &approved, &mut store)
        .await
        .unwrap();
    assert_eq!(record.outcome(), ValidationOutcome::Failed);
    drop(store);

    let reopened = ValidationEvidenceStore::open(&root, &[guest_root]).unwrap();
    let records = reopened.load_validation_records(&approved).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome(), ValidationOutcome::Failed);
    assert_eq!(records[0].receipt().unwrap().exit_code(), 1);
    assert!(
        reopened
            .find_reusable_validation(&approved)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
#[cfg(unix)]
async fn native_store_never_reuses_a_receipt_after_output_bytes_change() {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let root = temp.path().join("native-store");
    let guest_root = temp.path().join("guest-workspace");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&guest_root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&guest_root, fs::Permissions::from_mode(0o700)).unwrap();
    let fixture = fixture(ResultBehavior::Success);
    let server = start_mock_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let approved = plan();
    let mut store =
        ValidationEvidenceStore::open(&root, std::slice::from_ref(&guest_root)).unwrap();

    let record = run_approved_validation(&handle, &approved, &mut store)
        .await
        .unwrap();
    let reference = record.receipt().unwrap().output().reference();
    let token = reference.strip_prefix("validation-output:v1:").unwrap();
    let stdout_path = root.join("outputs").join(token).join("stdout.bin");
    fs::write(&stdout_path, b"tampered output").unwrap();
    drop(store);

    let reopened = ValidationEvidenceStore::open(&root, &[guest_root]).unwrap();
    assert!(reopened.find_reusable_validation(&approved).is_err());

    fs::remove_file(&stdout_path).unwrap();
    let external_output = temp.path().join("guest-workspace").join("stdout.bin");
    fs::write(&external_output, b"outside evidence root").unwrap();
    symlink(external_output, &stdout_path).unwrap();
    assert!(reopened.find_reusable_validation(&approved).is_err());
}

#[tokio::test]
async fn nonzero_exit_overrides_passing_looking_output_and_receipt_is_not_reused() {
    let fixture = fixture(ResultBehavior::NonzeroWithPassingLookingOutput);
    let server = start_mock_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let approved = plan();
    let mut output_store = MemoryOutputStore::default();

    let record = run_approved_validation(&handle, &approved, &mut output_store)
        .await
        .unwrap();

    assert_eq!(record.outcome(), ValidationOutcome::Failed);
    let receipt = record
        .receipt()
        .expect("clean failed execution is recorded");
    assert_eq!(receipt.exit_code(), 1);
    assert_eq!(receipt.outcome(), ValidationOutcome::Failed);
    assert!(find_reusable_validation(&approved, std::slice::from_ref(receipt)).is_none());
}

#[tokio::test]
async fn missing_end_stream_error_and_sandbox_mismatch_fail_closed() {
    for (behavior, expected) in [
        (ResultBehavior::MissingEnd, ValidationOutcome::Unknown),
        (ResultBehavior::DuplicateExit, ValidationOutcome::Unknown),
        (ResultBehavior::StreamError, ValidationOutcome::Unknown),
        (
            ResultBehavior::UnknownOutputStream,
            ValidationOutcome::Unknown,
        ),
        (ResultBehavior::EmptySandboxId, ValidationOutcome::Unknown),
        (ResultBehavior::WrongSandbox, ValidationOutcome::Failed),
    ] {
        let fixture = fixture(behavior);
        let server = start_mock_server(fixture).await;
        let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
        let approved = plan();
        let mut output_store = MemoryOutputStore::default();

        let record = run_approved_validation(&handle, &approved, &mut output_store)
            .await
            .unwrap();

        assert_eq!(record.outcome(), expected);
        if expected == ValidationOutcome::Unknown {
            assert!(record.receipt().is_none());
        } else {
            let receipt = record.receipt().unwrap();
            assert_eq!(receipt.sandbox_id(), "different-sandbox");
            assert!(find_reusable_validation(&approved, std::slice::from_ref(receipt)).is_none());
        }
    }
}

#[tokio::test]
async fn dispatch_without_a_server_assigned_job_id_issues_no_validation_record() {
    for behavior in [ResultBehavior::MissingJobId, ResultBehavior::InvalidJobId] {
        let fixture = fixture(behavior);
        let server = start_mock_server(fixture).await;
        let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
        let approved = plan();
        let mut output_store = MemoryOutputStore::default();

        let error = run_approved_validation(&handle, &approved, &mut output_store)
            .await
            .unwrap_err();

        assert_eq!(
            error,
            lookback_tauri_lib::coding_agent::validation::ValidationRunError::Dispatch(
                lookback_tauri_lib::coding_agent::ValidationDispatchError
            )
        );
        assert!(output_store.output.is_none());
        assert!(output_store.records.is_empty());
    }
}

#[tokio::test]
async fn output_truncation_never_produces_a_passed_receipt() {
    let fixture = fixture(ResultBehavior::OversizedOutput);
    let server = start_mock_server(fixture).await;
    let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
    let approved = plan();
    let mut output_store = MemoryOutputStore::default();

    let record = run_approved_validation(&handle, &approved, &mut output_store)
        .await
        .unwrap();

    assert_eq!(record.outcome(), ValidationOutcome::Unknown);
    assert!(record.receipt().is_none());
    assert!(output_store.output.is_none());
}

#[test]
fn not_run_and_binding_changes_cannot_become_reusable_success() {
    let approved = plan();
    let not_run = record_validation_not_run(&approved);
    assert_eq!(not_run.outcome(), ValidationOutcome::NotRun);
    assert!(not_run.receipt().is_none());
    let optional = record_validation_not_run(&plan_with(
        "run-1",
        "attempt-1",
        "optional-check",
        false,
        approved.argv().to_vec(),
        approved.target().clone(),
    ));
    assert!(!optional.required());
    assert_eq!(optional.outcome(), ValidationOutcome::NotRun);

    let changed_command = ApprovedValidationPlan::new(
        "run-1",
        "attempt-1",
        "check-1",
        true,
        "sandbox-worker",
        EXPECTED_SANDBOX_ID,
        vec!["cargo".to_owned(), "test".to_owned(), "--all".to_owned()],
        "/workspace/repository",
        BTreeMap::from([("RUST_BACKTRACE".to_owned(), "0".to_owned())]),
        "d".repeat(64),
        Some(60_000),
        approved.target().clone(),
    )
    .unwrap();
    assert_ne!(
        changed_command.command_definition_digest(),
        approved.command_definition_digest()
    );
}

#[tokio::test]
#[cfg(unix)]
async fn coordinator_finalization_reopens_a_real_validation_receipt_before_accepting_it() {
    let fixture = coordinator_finalization_fixture(
        Some(ResultBehavior::Success),
        false,
        ValidationState::Passed,
    )
    .await;
    let validation_receipt = fixture
        .validation_record
        .as_ref()
        .unwrap()
        .receipt()
        .unwrap()
        .clone();
    assert_eq!(
        fixture.validation_record.as_ref().unwrap().outcome(),
        ValidationOutcome::Passed
    );
    let completed = start_finalization_fixture(&fixture).await;

    assert_eq!(completed.state, RunState::AwaitingReview);
    let assessment = completed.execution_evidence.as_ref().unwrap();
    assert!(assessment.validation.required);
    assert_eq!(
        assessment.validation.state,
        lookback_tauri_lib::coding_agent::ValidationState::Passed
    );
    let artifact_reference = completed
        .evidence
        .iter()
        .find(|reference| reference.name == "verified-sealed-artifact")
        .expect("verified artifact should be persisted after reopening validation evidence");
    let artifact_evidence: serde_json::Value = serde_json::from_slice(
        &fixture
            .coordinator
            .read_evidence(&fixture.snapshot.run_id, &artifact_reference.evidence_id)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        artifact_evidence["validation_receipt_digests"][0],
        sha256_hex(&serde_json::to_vec(&validation_receipt).unwrap())
    );

    let mut missing_adapter_receipt = fixture.typed.clone();
    missing_adapter_receipt.validation_receipts.clear();
    assert!(
        fixture
            .finalization_sources
            .reopen(
                &fixture.snapshot,
                &fixture.contract,
                &missing_adapter_receipt
            )
            .await
            .is_err()
    );
}

#[tokio::test]
#[cfg(unix)]
async fn verified_artifact_allows_approved_publication_when_required_validation_is_not_run() {
    let fixture = coordinator_finalization_fixture(None, true, ValidationState::Passed).await;
    let verifying = start_finalization_fixture(&fixture).await;

    assert_eq!(verifying.state, RunState::Verifying);
    let assessment = verifying.execution_evidence.as_ref().unwrap();
    assert!(assessment.validation.required);
    assert_eq!(assessment.validation.state, ValidationState::NotRun);
    let artifact_reference = verifying
        .evidence
        .iter()
        .find(|reference| reference.name == "verified-sealed-artifact")
        .expect("the host-verified sealed artifact remains a publication prerequisite");
    let artifact_evidence: serde_json::Value = serde_json::from_slice(
        &fixture
            .coordinator
            .read_evidence(&fixture.snapshot.run_id, &artifact_reference.evidence_id)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        artifact_evidence["validation_receipt_digests"],
        serde_json::json!([])
    );

    let published = fixture
        .coordinator
        .publish(&fixture.snapshot.run_id)
        .await
        .expect("NotRun validation alone must not override approved publication");

    assert_eq!(published.state, RunState::AwaitingReview);
    assert_ne!(published.state, RunState::Done);
    assert_eq!(
        fixture
            .publication_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
#[cfg(unix)]
async fn reopened_host_validation_failure_still_blocks_approved_publication() {
    let fixture = coordinator_finalization_fixture(
        Some(ResultBehavior::NonzeroWithPassingLookingOutput),
        true,
        ValidationState::Passed,
    )
    .await;
    let finalized = start_finalization_fixture(&fixture).await;

    let validation_record = fixture.validation_record.as_ref().unwrap();
    assert_eq!(validation_record.outcome(), ValidationOutcome::Failed);
    assert_eq!(
        validation_record.receipt().unwrap().outcome(),
        ValidationOutcome::Failed
    );
    let assessment = finalized.execution_evidence.as_ref().unwrap();
    assert!(assessment.validation.required);
    assert_eq!(assessment.validation.state, ValidationState::Failed);
    assert!(
        fixture
            .coordinator
            .publish(&fixture.snapshot.run_id)
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .publication_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
#[cfg(unix)]
async fn one_shot_host_validation_failure_is_sticky_over_reopened_passed_or_not_run_results() {
    for validation_behavior in [Some(ResultBehavior::Success), None] {
        let fixture =
            coordinator_finalization_fixture(validation_behavior, true, ValidationState::Failed)
                .await;
        let finalized = start_finalization_fixture(&fixture).await;

        assert_eq!(finalized.state, RunState::Failed);
        assert_ne!(finalized.state, RunState::Done);
        assert_eq!(
            fixture.typed.validation_receipts.len(),
            usize::from(validation_behavior.is_some())
        );
        assert_eq!(
            fixture
                .verified_finalization_reopens
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let assessment = finalized.execution_evidence.as_ref().unwrap();
        assert_eq!(assessment.validation.state, ValidationState::Failed);
        assert!(
            fixture
                .coordinator
                .publish(&fixture.snapshot.run_id)
                .await
                .is_err()
        );
        assert_eq!(
            fixture
                .publication_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }
}

#[tokio::test]
#[cfg(unix)]
async fn mismatched_typed_finalization_identity_never_reaches_verified_sources() {
    let alter_identity: [fn(&mut ExecutionIdentity); 7] = [
        |identity| identity.run_id.push_str("-other"),
        |identity| identity.attempt_id.push_str("-other"),
        |identity| identity.workflow_execution_id.push_str("-other"),
        |identity| identity.parent_job_id.push_str("-other"),
        |identity| identity.child_sandbox_job_id.push_str("-other"),
        |identity| identity.worker_id += 1,
        |identity| identity.sandbox_job_id.push_str("-other"),
    ];
    for alter in alter_identity {
        let fixture = coordinator_finalization_fixture(None, true, ValidationState::Passed).await;
        let mut typed = fixture.typed.clone();
        alter(&mut typed.identity);
        *fixture.execution.typed.lock().await = typed;

        let finalized = start_finalization_fixture(&fixture).await;

        assert_eq!(finalized.state, RunState::Unknown);
        assert_eq!(
            fixture
                .verified_finalization_reopens
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(
            fixture
                .coordinator
                .publish(&fixture.snapshot.run_id)
                .await
                .is_err()
        );
        assert_eq!(
            fixture
                .publication_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn semantic_judge_off_keeps_actual_provider_idle_and_preserves_human_review() {
    let settings = claimed_judge_settings();
    let provider = judge_provider::provider_selection_for_settings(&settings).unwrap();
    let contract = claimed_finalization_contract(provider);
    let mut snapshot = finalization_snapshot();
    snapshot.semantic_judge_enabled = false;
    snapshot.auto_done = false;
    let assessment_directory = tempfile::tempdir().unwrap();
    let engine = Arc::new(AssessmentEngine::new(
        AssessmentStore::new(assessment_directory.path()).unwrap(),
    ));
    let rpc = claimed_rpc::JudgeRpcFixture::new("gpt-4o-2024-08-06");
    let server = claimed_rpc::start_server(rpc.clone()).await;
    let claimed = ClaimedAssessmentConfig::new(
        engine,
        server.endpoint.clone(),
        settings,
        Duration::from_secs(30),
    );
    let fixture = coordinator_finalization_fixture_for(
        snapshot,
        contract,
        Some(ResultBehavior::Success),
        false,
        ValidationState::Passed,
        Some(claimed.clone()),
        Some(claimed_finalization_report()),
    )
    .await;
    start_finalization_fixture(&fixture).await;
    let run = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let run = fixture
                .coordinator
                .get_run(&fixture.snapshot.run_id)
                .unwrap()
                .unwrap();
            if run.state != RunState::Verifying {
                break run;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(run.state, RunState::AwaitingReview);
    assert!(
        !run.execution_evidence
            .as_ref()
            .unwrap()
            .acceptance_conditions_met
    );
    assert_eq!(rpc.enqueue_count(), 0);
    let recovered = fixture
        .coordinator
        .resume_finalization(&fixture.snapshot.run_id)
        .await
        .unwrap();
    assert_eq!(recovered.state, RunState::AwaitingReview);
    assert_eq!(rpc.enqueue_count(), 0);
    assert!(
        fixture
            .store
            .evidence(&fixture.snapshot.run_id)
            .unwrap()
            .iter()
            .all(|reference| !reference.name.starts_with("semantic-prepared-"))
    );
    let interrupted = coordinator_finalization_fixture_for(
        fixture.snapshot.clone(),
        fixture.contract.clone(),
        None,
        false,
        ValidationState::Passed,
        Some(claimed),
        Some(claimed_finalization_report()),
    )
    .await;
    let PreparationOutcome::Ready(created) = interrupted
        .coordinator
        .prepare_run_with_assessment_contract(
            interrupted.snapshot.clone(),
            interrupted.contract.clone(),
        )
        .await
        .unwrap()
    else {
        panic!("OFF recovery fixture should be ready");
    };
    let approved = interrupted
        .coordinator
        .approve_run(&created.snapshot.run_id)
        .unwrap();
    let preparing = interrupted
        .store
        .transition(
            &approved.snapshot.run_id,
            approved.revision,
            RunState::Preparing,
        )
        .unwrap();
    interrupted
        .store
        .start_attempt(
            &preparing.snapshot.run_id,
            preparing.revision,
            "attempt-1",
            "start-off-interrupted",
        )
        .unwrap();
    let current = interrupted
        .coordinator
        .get_run(&preparing.snapshot.run_id)
        .unwrap()
        .unwrap();
    let unknown = interrupted
        .store
        .transition(
            &current.snapshot.run_id,
            current.revision,
            RunState::Unknown,
        )
        .unwrap();
    assert_eq!(unknown.current_attempt_id.as_deref(), Some("attempt-1"));
    assert!(unknown.execution_evidence.is_none());
    let resumed = interrupted
        .coordinator
        .resume_finalization(&unknown.snapshot.run_id)
        .await
        .unwrap();
    assert_eq!(resumed.state, RunState::Unknown);
    assert!(resumed.execution_evidence.is_none());
    assert_eq!(rpc.enqueue_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn claimed_semantic_finalization_keeps_both_modes_finalizing_until_one_native_judge_result() {
    for mode in [ExecutionMode::OneShot, ExecutionMode::Interactive] {
        let settings = claimed_judge_settings();
        let provider = judge_provider::provider_selection_for_settings(&settings).unwrap();
        let contract = claimed_finalization_contract(provider);
        let mut snapshot = finalization_snapshot();
        snapshot.execution_mode = mode;
        snapshot.approval.as_mut().unwrap().applicable_modes = vec![mode];

        let assessment_directory = tempfile::tempdir().unwrap();
        let engine = Arc::new(AssessmentEngine::new(
            AssessmentStore::new(assessment_directory.path()).unwrap(),
        ));
        let rpc = claimed_rpc::JudgeRpcFixture::new("gpt-4o-2024-08-06");
        let rpc_server = claimed_rpc::start_server(rpc.clone()).await;
        let claimed = ClaimedAssessmentConfig::new(
            engine.clone(),
            rpc_server.endpoint.clone(),
            settings,
            Duration::from_secs(30),
        );
        let fixture = coordinator_finalization_fixture_for(
            snapshot,
            contract,
            Some(ResultBehavior::Success),
            false,
            ValidationState::Passed,
            Some(claimed.clone()),
            Some(claimed_finalization_report()),
        )
        .await;

        let preparation = fixture
            .coordinator
            .prepare_run_with_assessment_contract(
                fixture.snapshot.clone(),
                fixture.contract.clone(),
            )
            .await
            .unwrap();
        let PreparationOutcome::Ready(created) = preparation else {
            panic!("the local host fixture should pass preflight: {preparation:?}")
        };
        fixture
            .coordinator
            .approve_run(&created.snapshot.run_id)
            .unwrap();
        let started = fixture
            .coordinator
            .start(&created.snapshot.run_id, None)
            .await
            .unwrap();
        let started_identity = match started {
            lookback_tauri_lib::coding_agent::StartOutcome::Started { identity, .. } => identity,
            other => panic!("execution should start with its real dispatch receipt: {other:?}"),
        };
        assert_eq!(started_identity, finalization_identity(&fixture.snapshot));
        let dispatch_evidence = fixture.store.evidence(&fixture.snapshot.run_id).unwrap();
        assert!(
            dispatch_evidence
                .iter()
                .any(|reference| reference.name == "dispatch-intent")
        );
        let receipt_reference = dispatch_evidence
            .iter()
            .find(|reference| reference.name == "dispatch-receipt")
            .expect("the accepted host dispatch receipt is durable");
        let receipt: StartReceipt = serde_json::from_slice(
            &fixture
                .store
                .read_evidence(&fixture.snapshot.run_id, &receipt_reference.evidence_id)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt.identity, finalization_identity(&fixture.snapshot));

        let finish = if mode == ExecutionMode::Interactive {
            let coordinator = fixture.coordinator.clone();
            let run_id = fixture.snapshot.run_id.clone();
            Some(tokio::spawn(async move {
                coordinator.finish_interactive(&run_id).await
            }))
        } else {
            None
        };
        if tokio::time::timeout(Duration::from_secs(10), rpc.wait_until_enqueued())
            .await
            .is_err()
        {
            let current = fixture
                .coordinator
                .get_run(&fixture.snapshot.run_id)
                .unwrap()
                .unwrap();
            let evidence = fixture
                .store
                .evidence(&fixture.snapshot.run_id)
                .unwrap()
                .into_iter()
                .map(|reference| reference.name)
                .collect::<Vec<_>>();
            let inconclusive = fixture
                .store
                .evidence(&fixture.snapshot.run_id)
                .unwrap()
                .into_iter()
                .find(|reference| reference.name == "semantic-assessment-inconclusive")
                .and_then(|reference| {
                    fixture
                        .store
                        .read_evidence(&fixture.snapshot.run_id, &reference.evidence_id)
                        .ok()
                });
            panic!(
                "native judge RPC was not reached: state={:?}, evidence={evidence:?}, detail={:?}",
                current.state,
                inconclusive.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            );
        }

        let finalizing = fixture
            .coordinator
            .get_run(&fixture.snapshot.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(finalizing.state, RunState::Finalizing);
        assert!(finalizing.execution_evidence.is_none());
        let binding = fixture
            .store
            .assessment_intent(&fixture.snapshot.run_id, "attempt-1")
            .expect("the approved finalization inputs must already be claimed");
        let saved_base = fixture
            .store
            .read_saved_verified_finalization_inputs(&fixture.snapshot.run_id, "attempt-1")
            .expect("the typed host inputs must be durably indexed before dispatch");
        assert_eq!(binding.attempt_id, "attempt-1");
        assert_eq!(
            binding.judge_input_evidence_id,
            fixture
                .store
                .evidence(&fixture.snapshot.run_id)
                .unwrap()
                .iter()
                .find(|evidence| evidence.name == "judge-input")
                .unwrap()
                .evidence_id
        );
        assert!(!saved_base.index_reference().evidence_id.is_empty());

        let chat = rpc.captured_chat_args();
        assert_eq!(chat.messages.len(), 2);
        assert_eq!(
            chat.messages[0].role,
            lookback_tauri_lib::grpc::proto::jobworkerp::runner::llm::llm_chat_args::ChatRole::System as i32
        );
        assert_eq!(
            chat.messages[1].role,
            lookback_tauri_lib::grpc::proto::jobworkerp::runner::llm::llm_chat_args::ChatRole::User
                as i32
        );
        assert!(matches!(
            chat.messages[1].content.as_ref().and_then(|content| content.content.as_ref()),
            Some(lookback_tauri_lib::grpc::proto::jobworkerp::runner::llm::llm_chat_args::message_content::Content::Text(text))
                if text.contains("The host verified the approved validation receipt.")
                    && !text.contains("credential")
        ));
        assert_eq!(rpc.enqueue_count(), 1);

        rpc.release_response();
        if let Some(finish) = finish {
            finish.await.unwrap().unwrap();
        }
        let completed =
            wait_for_finalization_state(&fixture.coordinator, &fixture.snapshot.run_id).await;
        assert_eq!(completed.state, RunState::AwaitingReview);
        let final_assessment = completed.execution_evidence.as_ref().unwrap();
        assert!(final_assessment.acceptance_conditions_met);
        assert_eq!(final_assessment.validation.state, ValidationState::Passed);
        assert!(final_assessment.report_save_confirmed);
        assert!(final_assessment.commit_consistent);
        assert_eq!(rpc.enqueue_count(), 1);

        let resumed = CodingAgentCoordinator::new(
            Arc::clone(&fixture.store),
            Some(fixture.execution.clone()),
            None,
        )
        .with_verified_finalization(fixture.finalization_sources.clone())
        .with_claimed_assessment(claimed);
        assert_eq!(
            resumed
                .resume_finalization(&fixture.snapshot.run_id)
                .await
                .unwrap()
                .state,
            RunState::AwaitingReview
        );
        assert_eq!(rpc.enqueue_count(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn claimed_finalization_resume_composes_terminal_judgment_without_a_second_enqueue() {
    let settings = claimed_judge_settings();
    let provider = judge_provider::provider_selection_for_settings(&settings).unwrap();
    let contract = claimed_finalization_contract(provider);
    let assessment_directory = tempfile::tempdir().unwrap();
    let engine = Arc::new(AssessmentEngine::new(
        AssessmentStore::new(assessment_directory.path()).unwrap(),
    ));
    let rpc = claimed_rpc::JudgeRpcFixture::new("gpt-4o-2024-08-06");
    let rpc_server = claimed_rpc::start_server(rpc.clone()).await;
    let claimed = ClaimedAssessmentConfig::new(
        engine.clone(),
        rpc_server.endpoint.clone(),
        settings,
        Duration::from_secs(30),
    );
    let fixture = coordinator_finalization_fixture_for(
        finalization_snapshot(),
        contract,
        Some(ResultBehavior::Success),
        false,
        ValidationState::Passed,
        Some(claimed.clone()),
        Some(claimed_finalization_report()),
    )
    .await;
    let trigger_connection = rusqlite::Connection::open(
        DataPaths::with_root(&fixture.data_root).coding_agent_runs_db_path(),
    )
    .unwrap();
    trigger_connection
        .execute_batch(
            "CREATE TRIGGER fail_final_execution_insert
             BEFORE INSERT ON agent_run_execution_evidence
             BEGIN SELECT RAISE(ABORT, 'injected final-row interruption'); END;",
        )
        .unwrap();

    let PreparationOutcome::Ready(created) = fixture
        .coordinator
        .prepare_run_with_assessment_contract(fixture.snapshot.clone(), fixture.contract.clone())
        .await
        .unwrap()
    else {
        panic!("the local host fixture should pass preflight")
    };
    fixture
        .coordinator
        .approve_run(&created.snapshot.run_id)
        .unwrap();
    fixture
        .coordinator
        .start(&created.snapshot.run_id, None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), rpc.wait_until_enqueued())
        .await
        .expect("native judge RPC should be reached");
    rpc.release_response();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let evidence = fixture.store.evidence(&fixture.snapshot.run_id).unwrap();
            if evidence.iter().any(|reference| {
                reference
                    .name
                    .starts_with("semantic-assessment-assessment-")
            }) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the terminal judge result should be durable before final-row recovery");
    assert_eq!(rpc.enqueue_count(), 1);
    let claim = fixture
        .store
        .assessment_intent(&fixture.snapshot.run_id, "attempt-1")
        .unwrap();
    let prepared_before = engine
        .load_saved_prepared_for_claim(&fixture.store, &claim)
        .unwrap();
    let prepared_metadata_reference = fixture
        .store
        .evidence(&fixture.snapshot.run_id)
        .unwrap()
        .into_iter()
        .find(|reference| reference.name == format!("semantic-prepared-{}", claim.assessment_id))
        .unwrap();
    let prepared_metadata_before = fixture
        .store
        .read_evidence(
            &fixture.snapshot.run_id,
            &prepared_metadata_reference.evidence_id,
        )
        .unwrap();
    let metadata: serde_json::Value = serde_json::from_slice(&prepared_metadata_before).unwrap();
    assert_eq!(metadata["assessment_id"], claim.assessment_id);
    assert_eq!(
        metadata["judge_input_digest"],
        prepared_before.judge_input_digest()
    );
    let original_deadline = metadata["deadline_unix_ms"].as_u64().unwrap();
    assert!(original_deadline > 0);
    let interrupted = fixture
        .coordinator
        .get_run(&fixture.snapshot.run_id)
        .unwrap()
        .unwrap();
    assert_eq!(interrupted.state, RunState::Finalizing);
    assert!(interrupted.execution_evidence.is_none());
    trigger_connection
        .execute_batch("DROP TRIGGER fail_final_execution_insert;")
        .unwrap();

    let restarted_engine = Arc::new(AssessmentEngine::new(
        AssessmentStore::new(assessment_directory.path()).unwrap(),
    ));
    let restarted_assessment = ClaimedAssessmentConfig::new(
        Arc::clone(&restarted_engine),
        rpc_server.endpoint.clone(),
        claimed_judge_settings(),
        Duration::from_secs(30),
    );
    let restarted = CodingAgentCoordinator::new(
        Arc::clone(&fixture.store),
        Some(fixture.execution.clone()),
        None,
    )
    .with_verified_finalization(fixture.finalization_sources.clone())
    .with_claimed_assessment(restarted_assessment);
    let recovered = restarted
        .resume_finalization(&fixture.snapshot.run_id)
        .await
        .unwrap();
    assert_eq!(recovered.state, RunState::AwaitingReview);
    assert!(
        recovered
            .execution_evidence
            .as_ref()
            .unwrap()
            .acceptance_conditions_met
    );
    let prepared_after = restarted_engine
        .load_saved_prepared_for_claim(&fixture.store, &claim)
        .unwrap();
    assert_eq!(prepared_after, prepared_before);
    let metadata_after = fixture
        .store
        .read_evidence(
            &fixture.snapshot.run_id,
            &prepared_metadata_reference.evidence_id,
        )
        .unwrap();
    let recovered_metadata: serde_json::Value = serde_json::from_slice(&metadata_after).unwrap();
    assert_eq!(recovered_metadata["deadline_unix_ms"], original_deadline);
    assert_eq!(recovered_metadata["assessment_id"], claim.assessment_id);
    assert_eq!(metadata_after, prepared_metadata_before);
    let terminal_assessment_evidence = fixture
        .store
        .evidence(&fixture.snapshot.run_id)
        .unwrap()
        .into_iter()
        .filter(|reference| {
            reference
                .name
                .starts_with(&format!("semantic-assessment-{}-", claim.assessment_id))
        })
        .collect::<Vec<_>>();
    assert_eq!(terminal_assessment_evidence.len(), 1);
    assert_eq!(rpc.enqueue_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn pending_claimed_judgment_stays_finalizing_then_resumes_to_terminal_without_reenqueue() {
    for mode in [ExecutionMode::OneShot, ExecutionMode::Interactive] {
        let settings = claimed_judge_settings();
        let provider = judge_provider::provider_selection_for_settings(&settings).unwrap();
        let contract = claimed_finalization_contract(provider);
        let mut snapshot = finalization_snapshot();
        snapshot.execution_mode = mode;
        snapshot.approval.as_mut().unwrap().applicable_modes = vec![mode];

        let assessment_directory = tempfile::tempdir().unwrap();
        let engine = Arc::new(AssessmentEngine::new(
            AssessmentStore::new(assessment_directory.path()).unwrap(),
        ));
        let rpc = claimed_rpc::JudgeRpcFixture::new_recoverable("gpt-4o-2024-08-06");
        let rpc_server = claimed_rpc::start_server(rpc.clone()).await;
        let claimed = ClaimedAssessmentConfig::new(
            Arc::clone(&engine),
            rpc_server.endpoint.clone(),
            settings,
            Duration::from_secs(30),
        );
        let fixture = coordinator_finalization_fixture_for(
            snapshot,
            contract,
            Some(ResultBehavior::Success),
            false,
            ValidationState::Passed,
            Some(claimed),
            Some(claimed_finalization_report()),
        )
        .await;
        prepare_and_start_claimed_fixture(&fixture).await;

        if mode == ExecutionMode::Interactive {
            fixture
                .coordinator
                .finish_interactive(&fixture.snapshot.run_id)
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(10), rpc.wait_until_result_lookups(1))
            .await
            .expect("the stream loss should produce one initial result lookup");
        let claim = fixture
            .store
            .assessment_intent(&fixture.snapshot.run_id, "attempt-1")
            .unwrap();
        let prepared_before = engine
            .load_saved_prepared_for_claim(&fixture.store, &claim)
            .unwrap();
        let saved_inputs_before = fixture
            .store
            .read_saved_verified_finalization_inputs(&fixture.snapshot.run_id, "attempt-1")
            .unwrap();
        let saved_base_bytes_before = fixture
            .store
            .read_evidence(
                &fixture.snapshot.run_id,
                &saved_inputs_before.index_reference().evidence_id,
            )
            .unwrap();
        let metadata_reference = fixture
            .store
            .evidence(&fixture.snapshot.run_id)
            .unwrap()
            .into_iter()
            .find(|reference| {
                reference.name == format!("semantic-prepared-{}", claim.assessment_id)
            })
            .unwrap();
        let prepared_metadata_before = fixture
            .store
            .read_evidence(&fixture.snapshot.run_id, &metadata_reference.evidence_id)
            .unwrap();
        let manifest_reference = fixture
            .store
            .evidence(&fixture.snapshot.run_id)
            .unwrap()
            .into_iter()
            .find(|reference| {
                reference.evidence_id == claim.snapshot_manifest_evidence_id
                    && reference.name == "snapshot-manifest"
            })
            .unwrap();
        let manifest_bytes_before = fixture
            .store
            .read_evidence(&fixture.snapshot.run_id, &manifest_reference.evidence_id)
            .unwrap();
        let pending_name = "semantic-pending-".to_owned();
        let pending_evidence_saved = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if fixture
                    .store
                    .evidence(&fixture.snapshot.run_id)
                    .unwrap()
                    .iter()
                    .any(|reference| reference.name.starts_with(&pending_name))
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        if !pending_evidence_saved {
            let run = fixture
                .coordinator
                .get_run(&fixture.snapshot.run_id)
                .unwrap()
                .unwrap();
            let evidence = fixture.store.evidence(&fixture.snapshot.run_id).unwrap();
            panic!(
                "pending outcome was not content-addressed as Pending: state={:?}, evidence={:?}",
                run.state,
                evidence
                    .iter()
                    .map(|item| item.name.as_str())
                    .collect::<Vec<_>>()
            );
        }
        let pending = fixture
            .coordinator
            .get_run(&fixture.snapshot.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(pending.state, RunState::Finalizing);
        assert!(pending.execution_evidence.is_none());
        if mode == ExecutionMode::Interactive {
            assert!(pending.interactive_finalization.is_none());
        }
        let pending_reference = fixture
            .store
            .evidence(&fixture.snapshot.run_id)
            .unwrap()
            .into_iter()
            .find(|reference| reference.name.starts_with(&pending_name))
            .unwrap();
        assert_eq!(
            pending_reference.name,
            format!("semantic-pending-{}", pending_reference.sha256)
        );
        let pending_record: serde_json::Value = serde_json::from_slice(
            &fixture
                .store
                .read_evidence(&fixture.snapshot.run_id, &pending_reference.evidence_id)
                .unwrap(),
        )
        .unwrap();
        assert!(pending_record["outcome"]["pending"].is_object());
        assert!(
            !pending_record["acceptance_conditions_met"]
                .as_bool()
                .unwrap()
        );
        assert_eq!(rpc.enqueue_count(), 1);

        let reopened_store =
            Arc::new(RunStore::open(&DataPaths::with_root(&fixture.data_root)).unwrap());
        let reopened_engine = Arc::new(AssessmentEngine::new(
            AssessmentStore::new(assessment_directory.path()).unwrap(),
        ));
        let reopened_config = ClaimedAssessmentConfig::new(
            Arc::clone(&reopened_engine),
            rpc_server.endpoint.clone(),
            claimed_judge_settings(),
            Duration::from_secs(30),
        );
        let resumed = CodingAgentCoordinator::new(
            Arc::clone(&reopened_store),
            Some(fixture.execution.clone()),
            None,
        )
        .with_verified_finalization(fixture.finalization_sources.clone())
        .with_claimed_assessment(reopened_config);
        let resume_task = {
            let resumed = resumed.clone();
            let run_id = fixture.snapshot.run_id.clone();
            tokio::spawn(async move { resumed.resume_finalization(&run_id).await })
        };
        tokio::time::timeout(Duration::from_secs(10), rpc.wait_until_result_lookups(2))
            .await
            .expect("recovery must look up the original job result");
        let still_pending = resumed.get_run(&fixture.snapshot.run_id).unwrap().unwrap();
        assert_eq!(still_pending.state, RunState::Finalizing);
        assert!(still_pending.execution_evidence.is_none());
        rpc.release_result_lookup();
        let recovered = resume_task.await.unwrap().unwrap();
        assert_eq!(recovered.state, RunState::AwaitingReview);
        assert!(
            recovered
                .execution_evidence
                .as_ref()
                .unwrap()
                .acceptance_conditions_met
        );
        if mode == ExecutionMode::Interactive {
            assert!(recovered.interactive_finalization.is_some());
        }
        assert_eq!(rpc.enqueue_count(), 1);
        assert_eq!(rpc.result_lookup_count(), 2);
        let prepared_after = reopened_engine
            .load_saved_prepared_for_claim(&reopened_store, &claim)
            .unwrap();
        assert_eq!(prepared_after, prepared_before);
        let saved_inputs_after = reopened_store
            .read_saved_verified_finalization_inputs(&fixture.snapshot.run_id, "attempt-1")
            .unwrap();
        assert_eq!(
            reopened_store
                .read_evidence(
                    &fixture.snapshot.run_id,
                    &saved_inputs_after.index_reference().evidence_id,
                )
                .unwrap(),
            saved_base_bytes_before
        );
        let reopened_metadata_reference = reopened_store
            .evidence(&fixture.snapshot.run_id)
            .unwrap()
            .into_iter()
            .find(|reference| reference.name == metadata_reference.name)
            .unwrap();
        assert_eq!(
            reopened_store
                .read_evidence(
                    &fixture.snapshot.run_id,
                    &reopened_metadata_reference.evidence_id,
                )
                .unwrap(),
            prepared_metadata_before
        );
        let reopened_manifest_reference = reopened_store
            .evidence(&fixture.snapshot.run_id)
            .unwrap()
            .into_iter()
            .find(|reference| reference.name == "snapshot-manifest")
            .unwrap();
        assert_eq!(
            reopened_store
                .read_evidence(
                    &fixture.snapshot.run_id,
                    &reopened_manifest_reference.evidence_id,
                )
                .unwrap(),
            manifest_bytes_before
        );
        let names = reopened_store
            .evidence(&fixture.snapshot.run_id)
            .unwrap()
            .into_iter()
            .map(|reference| reference.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names
                .iter()
                .filter(|name| name.starts_with(&pending_name))
                .count(),
            1
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| {
                    name.starts_with(&format!(
                        "semantic-assessment-{}-terminal",
                        claim.assessment_id
                    ))
                })
                .count(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn interactive_cancel_while_claimed_judge_is_blocked_fences_late_pass() {
    let settings = claimed_judge_settings();
    let provider = judge_provider::provider_selection_for_settings(&settings).unwrap();
    let contract = claimed_finalization_contract(provider);
    let assessment_directory = tempfile::tempdir().unwrap();
    let engine = Arc::new(AssessmentEngine::new(
        AssessmentStore::new(assessment_directory.path()).unwrap(),
    ));
    let rpc = claimed_rpc::JudgeRpcFixture::new("gpt-4o-2024-08-06");
    let rpc_server = claimed_rpc::start_server(rpc.clone()).await;
    let claimed = ClaimedAssessmentConfig::new(
        engine,
        rpc_server.endpoint.clone(),
        settings,
        Duration::from_secs(30),
    );
    let mut snapshot = finalization_snapshot();
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    let fixture = coordinator_finalization_fixture_for(
        snapshot,
        contract,
        Some(ResultBehavior::Success),
        true,
        ValidationState::Passed,
        Some(claimed),
        Some(claimed_finalization_report()),
    )
    .await;
    prepare_and_start_claimed_fixture(&fixture).await;
    *fixture.execution.cancellation_observation.lock().unwrap() = Some(CancellationObservation {
        process_exit: ProcessExitObservation::Killed,
        child_writers: ChildWritersStatus::Stopped,
    });
    let finish = {
        let coordinator = fixture.coordinator.clone();
        let run_id = fixture.snapshot.run_id.clone();
        tokio::spawn(async move { coordinator.finish_interactive(&run_id).await })
    };
    tokio::time::timeout(Duration::from_secs(10), rpc.wait_until_enqueued())
        .await
        .expect("the claimed judge call should reach the RPC barrier");

    let cancel = tokio::time::timeout(
        Duration::from_secs(1),
        fixture.coordinator.cancel(&fixture.snapshot.run_id),
    )
    .await;
    rpc.release_response();
    let finish_result = tokio::time::timeout(Duration::from_secs(10), finish)
        .await
        .expect("finish should release its lock while awaiting judge")
        .unwrap()
        .unwrap();
    assert!(
        cancel.is_ok(),
        "cancel must pass the durable fence promptly"
    );
    assert_eq!(cancel.unwrap().unwrap().state, RunState::Cancelled);
    assert_eq!(finish_result.state, RunState::Cancelled);
    let cancelled = fixture
        .coordinator
        .get_run(&fixture.snapshot.run_id)
        .unwrap()
        .unwrap();
    assert!(cancelled.execution_evidence.is_none());
    assert_eq!(
        fixture
            .publication_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(
        fixture
            .store
            .evidence(&fixture.snapshot.run_id)
            .unwrap()
            .iter()
            .all(|reference| {
                !reference.name.starts_with("semantic-assessment-")
                    && !reference.name.starts_with("semantic-pending-")
            })
    );
    assert_eq!(rpc.enqueue_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn cancel_during_resume_result_lookup_fences_late_terminal_assessment() {
    let settings = claimed_judge_settings();
    let provider = judge_provider::provider_selection_for_settings(&settings).unwrap();
    let contract = claimed_finalization_contract(provider);
    let assessment_directory = tempfile::tempdir().unwrap();
    let engine = Arc::new(AssessmentEngine::new(
        AssessmentStore::new(assessment_directory.path()).unwrap(),
    ));
    let rpc = claimed_rpc::JudgeRpcFixture::new_recoverable("gpt-4o-2024-08-06");
    let rpc_server = claimed_rpc::start_server(rpc.clone()).await;
    let claimed = ClaimedAssessmentConfig::new(
        engine,
        rpc_server.endpoint.clone(),
        settings,
        Duration::from_secs(30),
    );
    let mut snapshot = finalization_snapshot();
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    let fixture = coordinator_finalization_fixture_for(
        snapshot,
        contract,
        Some(ResultBehavior::Success),
        true,
        ValidationState::Passed,
        Some(claimed),
        Some(claimed_finalization_report()),
    )
    .await;
    prepare_and_start_claimed_fixture(&fixture).await;
    fixture
        .coordinator
        .finish_interactive(&fixture.snapshot.run_id)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), rpc.wait_until_result_lookups(1))
        .await
        .expect("the initial lost stream should leave a recoverable pending claim");
    assert!(
        fixture
            .coordinator
            .get_run(&fixture.snapshot.run_id)
            .unwrap()
            .unwrap()
            .execution_evidence
            .is_none()
    );

    let reopened_store =
        Arc::new(RunStore::open(&DataPaths::with_root(&fixture.data_root)).unwrap());
    let reopened_engine = Arc::new(AssessmentEngine::new(
        AssessmentStore::new(assessment_directory.path()).unwrap(),
    ));
    let resumed = CodingAgentCoordinator::new(
        Arc::clone(&reopened_store),
        Some(fixture.execution.clone()),
        None,
    )
    .with_verified_finalization(fixture.finalization_sources.clone())
    .with_claimed_assessment(ClaimedAssessmentConfig::new(
        reopened_engine,
        rpc_server.endpoint.clone(),
        claimed_judge_settings(),
        Duration::from_secs(30),
    ));
    *fixture.execution.cancellation_observation.lock().unwrap() = Some(CancellationObservation {
        process_exit: ProcessExitObservation::Killed,
        child_writers: ChildWritersStatus::Stopped,
    });
    let resume_task = {
        let resumed = resumed.clone();
        let run_id = fixture.snapshot.run_id.clone();
        tokio::spawn(async move { resumed.resume_finalization(&run_id).await })
    };
    tokio::time::timeout(Duration::from_secs(10), rpc.wait_until_result_lookups(2))
        .await
        .expect("resumption must block only on lookup of the original job");
    let cancel = tokio::time::timeout(
        Duration::from_secs(1),
        resumed.cancel(&fixture.snapshot.run_id),
    )
    .await;
    rpc.release_result_lookup();
    let resume_result = tokio::time::timeout(Duration::from_secs(10), resume_task)
        .await
        .expect("resume should finish after cancellation and lookup release")
        .unwrap()
        .unwrap();
    assert!(
        cancel.is_ok(),
        "cancel must pass the durable fence promptly"
    );
    assert_eq!(cancel.unwrap().unwrap().state, RunState::Cancelled);
    assert_eq!(resume_result.state, RunState::Cancelled);
    let cancelled = resumed.get_run(&fixture.snapshot.run_id).unwrap().unwrap();
    assert!(cancelled.execution_evidence.is_none());
    assert_eq!(rpc.enqueue_count(), 1);
    assert_eq!(rpc.result_lookup_count(), 2);
    let semantic_evidence = reopened_store
        .evidence(&fixture.snapshot.run_id)
        .unwrap()
        .into_iter()
        .filter(|reference| {
            reference.name.starts_with("semantic-pending-") || reference.name.ends_with("-terminal")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        semantic_evidence
            .iter()
            .filter(|reference| reference.name.starts_with("semantic-pending-"))
            .count(),
        1,
        "the original pending observation remains durable"
    );
    assert!(
        semantic_evidence
            .iter()
            .all(|reference| !reference.name.ends_with("-terminal")),
        "cancelled late lookup must not persist terminal semantic evidence"
    );
}

async fn prepare_and_start_claimed_fixture(fixture: &CoordinatorFinalizationFixture) {
    let prepared = fixture
        .coordinator
        .prepare_run_with_assessment_contract(fixture.snapshot.clone(), fixture.contract.clone())
        .await
        .unwrap();
    let PreparationOutcome::Ready(created) = prepared else {
        panic!("claimed fixture must pass preflight: {prepared:?}")
    };
    fixture
        .coordinator
        .approve_run(&created.snapshot.run_id)
        .unwrap();
    assert!(matches!(
        fixture
            .coordinator
            .start(&created.snapshot.run_id, None)
            .await
            .unwrap(),
        lookback_tauri_lib::coding_agent::StartOutcome::Started { .. }
    ));
}

async fn wait_for_finalization_state(
    coordinator: &CodingAgentCoordinator,
    run_id: &RunId,
) -> lookback_tauri_lib::coding_agent::AgentRunRecord {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let run = coordinator.get_run(run_id).unwrap().unwrap();
            if matches!(
                run.state,
                RunState::AwaitingReview | RunState::Failed | RunState::Unknown
            ) {
                return run;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("finalization should settle")
}

fn claimed_judge_settings() -> lookback_tauri_lib::commands::llm_settings::LlmSettings {
    use lookback_tauri_lib::commands::llm_settings::{LlmMode, LlmSettings};

    LlmSettings {
        mode: LlmMode::External,
        provider_model: Some("gpt-4o-2024-08-06".to_owned()),
        base_url: Some("https://llm.example.test/v1".to_owned()),
        ..LlmSettings::default()
    }
}

fn claimed_finalization_contract(provider: ProviderSelection) -> ApprovalAssessmentContract {
    let validation = ApprovalValidationDraft {
        required: true,
        command: Some(ApprovalAssessmentCommand {
            argv: vec!["cargo".into(), "test".into(), "--workspace".into()],
            working_directory: "/workspace/repository".into(),
            permissions: vec![ValidationPermission::ReadWorkspace],
            profile_digest: "d".repeat(64),
            timeout_ms: 60_000,
        }),
    };
    ApprovalAssessmentContract::from_issue_acceptance(
        RunPurpose::Implementation,
        vec!["The approved behavior is present.".into()],
        vec![ApprovalCriterionPolicy {
            required: true,
            method: CriterionEvaluationMethod::SemanticJudge,
        }],
        vec![validation],
        Some(provider),
        Some("coding-agent-judge-v1".into()),
        AssessmentInputScope {
            allowed_fields: vec![coding_agent::model::ApprovedJudgeInputField::Report],
            allowed_sources: vec![coding_agent::assessment::AllowedEvidenceSource {
                kind: coding_agent::assessment::EvidenceSourceKind::Report,
                identity: "report:attempt-1".into(),
                max_bytes: 32 * 1024,
            }],
            max_input_bytes: 128 * 1024,
        },
    )
    .unwrap()
}

fn claimed_finalization_report() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "run_id": "run-1",
        "attempt_id": "attempt-1",
        "task_kind": "implementation",
        "status": "completed",
        "summary": "The host verified the approved validation receipt.",
        "acceptance": [{
            "criterion_id": "AC-1",
            "assessment": "met",
            "evidence": ["report:attempt-1"]
        }],
        "validation": [{
            "validation_id": "V-1",
            "claimed_result": "passed",
            "evidence": ["validation:V-1"],
            "reason": "The host validation receipt is available."
        }],
        "investigation": null,
        "implementation": {
            "change_summary": "Validated claimed finalization integration.",
            "changed_files": ["src/lib.rs"],
            "implementation_notes": []
        },
        "unresolved": []
    }))
    .unwrap()
}

struct CoordinatorFinalizationFixture {
    _temp: tempfile::TempDir,
    data_root: std::path::PathBuf,
    store: Arc<RunStore>,
    snapshot: AgentRunSnapshot,
    contract: ApprovalAssessmentContract,
    coordinator: CodingAgentCoordinator,
    finalization_sources: Arc<NativeExecutorFinalizationSources>,
    typed: TypedFinalizationEvidence,
    execution: Arc<FinalizationExecution>,
    validation_record: Option<ValidationRecord>,
    publication_calls: Arc<std::sync::atomic::AtomicUsize>,
    verified_finalization_reopens: Arc<std::sync::atomic::AtomicUsize>,
}

struct FinalizationPublication {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

struct CountingFinalizationSources {
    inner: Arc<NativeExecutorFinalizationSources>,
    reopens: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl VerifiedFinalizationSources for CountingFinalizationSources {
    async fn reopen(
        &self,
        snapshot: &AgentRunSnapshot,
        contract: &ApprovalAssessmentContract,
        evidence: &TypedFinalizationEvidence,
    ) -> Result<VerifiedFinalization, ExecutionResult> {
        self.reopens
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.reopen(snapshot, contract, evidence).await
    }

    async fn artifact_manifest_bytes(
        &self,
        run_id: &str,
        attempt_id: &str,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        self.inner.artifact_manifest_bytes(run_id, attempt_id).await
    }

    fn resolve_assessment_source(
        &self,
        run_id: &str,
        attempt_id: &str,
        source: &lookback_tauri_lib::coding_agent::assessment::AllowedEvidenceSource,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        self.inner
            .resolve_assessment_source(run_id, attempt_id, source)
    }
}

#[async_trait]
impl PublicationAdapter for FinalizationPublication {
    async fn preflight(
        &self,
        _snapshot: &AgentRunSnapshot,
    ) -> Result<PublicationSupport, ExecutionResult> {
        Ok(PublicationSupport::Verified {
            executor_version: EXECUTOR_VERSION.to_owned(),
            protocol_version: PROTOCOL_VERSION,
            features: BTreeSet::from([
                "prepare".to_owned(),
                "capture".to_owned(),
                "seal".to_owned(),
                "resolve".to_owned(),
                "artifact_only_publication".to_owned(),
                "push".to_owned(),
                "pull_request".to_owned(),
                "secret_scan_supported_formats_v1".to_owned(),
            ]),
            approved_base_branch: "main".to_owned(),
            approved_web_base_url: "https://forge.example".to_owned(),
        })
    }

    async fn publish(
        &self,
        request: &PublicationRequest,
    ) -> Result<PublicationOutcome, ExecutionResult> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(PublicationOutcome::BranchPublished {
            branch: request.branch.clone(),
            commit: request.frozen_commit.clone(),
            remote_head: request.frozen_commit.clone(),
        })
    }

    async fn reconcile(
        &self,
        _request: &PublicationRequest,
    ) -> Result<PublicationOutcome, ExecutionResult> {
        Ok(PublicationOutcome::Unknown)
    }
}

async fn coordinator_finalization_fixture(
    validation_behavior: Option<ResultBehavior>,
    publication_enabled: bool,
    host_validation_state: ValidationState,
) -> CoordinatorFinalizationFixture {
    coordinator_finalization_fixture_for(
        finalization_snapshot(),
        finalization_contract(),
        validation_behavior,
        publication_enabled,
        host_validation_state,
        None,
        None,
    )
    .await
}

async fn coordinator_finalization_fixture_for(
    mut snapshot: AgentRunSnapshot,
    contract: ApprovalAssessmentContract,
    validation_behavior: Option<ResultBehavior>,
    publication_enabled: bool,
    host_validation_state: ValidationState,
    claimed_assessment: Option<ClaimedAssessmentConfig>,
    report: Option<Vec<u8>>,
) -> CoordinatorFinalizationFixture {
    let temp = tempfile::tempdir().unwrap();
    let validation_root = create_private_dir(temp.path(), "validation-store");
    let guest_root = create_private_dir(temp.path(), "guest-workspace");
    let target =
        ValidationTargetSnapshot::new("b".repeat(40), "c".repeat(40), "3".repeat(64)).unwrap();
    let approved_validation = plan_with(
        "run-1",
        "attempt-1",
        "V-1",
        true,
        vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "--workspace".to_owned(),
        ],
        target.clone(),
    );
    let validation_record = if let Some(validation_behavior) = validation_behavior {
        let executor_fixture = fixture(validation_behavior);
        let server = start_mock_server(executor_fixture).await;
        let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
        let mut validation_store =
            ValidationEvidenceStore::open(&validation_root, std::slice::from_ref(&guest_root))
                .unwrap();
        Some(
            run_approved_validation(&handle, &approved_validation, &mut validation_store)
                .await
                .unwrap(),
        )
    } else {
        None
    };
    let validation_receipt = validation_record
        .as_ref()
        .and_then(|record| record.receipt().cloned());
    let reopened_validation_store = Arc::new(
        ValidationEvidenceStore::open(&validation_root, std::slice::from_ref(&guest_root)).unwrap(),
    );

    if publication_enabled {
        snapshot.publication_plan = PublicationPlan::PushBranch;
        snapshot.allowed_operations =
            vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
    }
    let stored_snapshot =
        StoredAgentRunSnapshot::new(snapshot.clone(), Some(contract.clone())).unwrap();
    let approved_snapshot_digest = sha256_hex(&serde_json::to_vec(&stored_snapshot).unwrap());
    let manifest_bytes = sealed_manifest_fixture_bytes();
    let manifest_digest = sha256_hex(&manifest_bytes);
    let repository = RepositoryId {
        host: snapshot.repository.identity.host.clone(),
        owner: snapshot.repository.identity.organization.clone(),
        name: snapshot.repository.identity.repository.clone(),
    };
    let artifact = ResolvedArtifact {
        run_id: snapshot.run_id.to_string(),
        attempt_id: "attempt-1".to_owned(),
        repository: repository.clone(),
        branch: snapshot.workspace_branch.clone(),
        base_branch: "main".to_owned(),
        base_commit: snapshot.base_commit.clone(),
        approved_snapshot_digest,
        scope_digest: "4".repeat(64),
        intent_approval_digest: "5".repeat(64),
        manifest_digest: manifest_digest.clone(),
        finalized_bytes_digest: "7".repeat(64),
        tree: target.full_tree_id().to_owned(),
        commit: target.full_commit_id().to_owned(),
        artifact_digest: "8".repeat(64),
        protocol_version: PROTOCOL_VERSION,
        executor_version: EXECUTOR_VERSION.to_owned(),
        capture_guarantee: CaptureGuarantee::HostIsolatedAfterSeal,
    };
    let native_executor = Arc::new(fake_native_executor(temp.path(), &artifact).await);
    let sealed_record = NativeSealedArtifactRecord {
        receipt: SealedArtifactReceipt {
            run_id: artifact.run_id.clone(),
            attempt_id: artifact.attempt_id.clone(),
            repository,
            base_branch: artifact.base_branch.clone(),
            base_commit: artifact.base_commit.clone(),
            approved_snapshot_digest: artifact.approved_snapshot_digest.clone(),
            scope_digest: artifact.scope_digest.clone(),
            intent_approval_digest: artifact.intent_approval_digest.clone(),
            manifest_digest: artifact.manifest_digest.clone(),
            finalized_bytes_digest: artifact.finalized_bytes_digest.clone(),
            tree: artifact.tree.clone(),
            commit: artifact.commit.clone(),
            artifact_digest: artifact.artifact_digest.clone(),
            protocol_version: PROTOCOL_VERSION,
            executor_version: EXECUTOR_VERSION.to_owned(),
            capture_guarantee: CaptureGuarantee::HostIsolatedAfterSeal,
        },
        target: target.clone(),
    };
    let finalization_sources = Arc::new(NativeExecutorFinalizationSources::new(
        native_executor,
        Arc::new(FixedSealedArtifactLookup(sealed_record)),
        reopened_validation_store,
        Arc::new(FixedValidationRuntime),
        Arc::new(EmptyAssessmentContext),
    ));
    let observation = HostFinalizationObservation {
        invocation_kind: ExecutionInvocationKind::DirectProcess,
        cli_exit: ProcessExitObservation::ExitedNormally { exit_code: 0 },
        shell_observation: ShellProcessObservation::NotSpawned,
        child_writers: ChildWritersStatus::Stopped,
        workspace_freeze: WorkspaceFreezeStatus::HostVerified {
            commit_id: target.full_commit_id().to_owned(),
        },
        report: Some(report.unwrap_or_else(finalization_report)),
        validation: ValidationEvidence {
            required: true,
            state: host_validation_state,
        },
        acceptance_conditions_met: true,
    };
    let typed = TypedFinalizationEvidence {
        identity: finalization_identity(&snapshot),
        invocation_kind: observation.invocation_kind,
        cli_exit: observation.cli_exit.clone(),
        shell_observation: observation.shell_observation.clone(),
        child_writers: observation.child_writers,
        workspace_freeze: observation.workspace_freeze.clone(),
        sealed_artifact: Some(SealedArtifactBinding {
            full_commit_id: target.full_commit_id().to_owned(),
            full_tree_id: target.full_tree_id().to_owned(),
            untracked_manifest_digest: target.untracked_manifest_digest().to_owned(),
            target_snapshot_digest: target.digest().to_owned(),
        }),
        validation_receipts: validation_receipt.iter().cloned().collect(),
    };
    let execution = Arc::new(FinalizationExecution {
        snapshot: snapshot.clone(),
        observation,
        typed: tokio::sync::Mutex::new(typed.clone()),
        cancellation_observation: std::sync::Mutex::new(None),
    });
    let data_root = temp.path().join("app-data");
    let app_store = Arc::new(RunStore::open(&DataPaths::with_root(&data_root)).unwrap());
    let publication_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let verified_finalization_reopens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let publication: Option<Arc<dyn PublicationAdapter>> = publication_enabled.then(|| {
        Arc::new(FinalizationPublication {
            calls: Arc::clone(&publication_calls),
        }) as Arc<dyn PublicationAdapter>
    });
    let counted_sources = Arc::new(CountingFinalizationSources {
        inner: Arc::clone(&finalization_sources),
        reopens: Arc::clone(&verified_finalization_reopens),
    });
    let mut coordinator =
        CodingAgentCoordinator::new(Arc::clone(&app_store), Some(execution.clone()), publication)
            .with_verified_finalization(counted_sources);
    if let Some(claimed_assessment) = claimed_assessment {
        coordinator = coordinator.with_claimed_assessment(claimed_assessment);
    }
    CoordinatorFinalizationFixture {
        _temp: temp,
        data_root,
        store: app_store,
        snapshot,
        contract,
        coordinator,
        finalization_sources,
        typed,
        execution,
        validation_record,
        publication_calls,
        verified_finalization_reopens,
    }
}

async fn start_finalization_fixture(
    fixture: &CoordinatorFinalizationFixture,
) -> lookback_tauri_lib::coding_agent::AgentRunRecord {
    let PreparationOutcome::Ready(created) = fixture
        .coordinator
        .prepare_run_with_assessment_contract(fixture.snapshot.clone(), fixture.contract.clone())
        .await
        .unwrap()
    else {
        panic!("test runtime should pass preflight")
    };
    fixture
        .coordinator
        .approve_run(&created.snapshot.run_id)
        .unwrap();
    fixture
        .coordinator
        .start(&created.snapshot.run_id, None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let run = fixture
                .coordinator
                .get_run(&fixture.snapshot.run_id)
                .unwrap()
                .unwrap();
            if matches!(
                run.state,
                RunState::Verifying
                    | RunState::AwaitingReview
                    | RunState::Failed
                    | RunState::Unknown
            ) {
                return run;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

struct FixedSealedArtifactLookup(NativeSealedArtifactRecord);

#[async_trait]
impl NativeSealedArtifactLookup for FixedSealedArtifactLookup {
    async fn lookup(
        &self,
        _run_id: &str,
        _attempt_id: &str,
    ) -> Result<Option<NativeSealedArtifactRecord>, ExecutionResult> {
        Ok(Some(self.0.clone()))
    }

    async fn manifest_bytes(
        &self,
        _run_id: &str,
        _attempt_id: &str,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        Ok(Some(sealed_manifest_fixture_bytes()))
    }
}

fn sealed_manifest_fixture_bytes() -> Vec<u8> {
    br#"{"fixture":"sealed"}"#.to_vec()
}

struct FixedValidationRuntime;

impl ValidationPlanRuntimeSource for FixedValidationRuntime {
    fn runtime_for(
        &self,
        _snapshot: &AgentRunSnapshot,
        _attempt_id: &str,
        definition: &lookback_tauri_lib::coding_agent::model::ApprovedValidationDefinition,
    ) -> Result<Option<ValidationPlanRuntime>, ExecutionResult> {
        if definition.validation_id != "V-1" {
            return Err(ExecutionResult::Conflict);
        }
        Ok(Some(ValidationPlanRuntime {
            worker_name: "sandbox-worker".to_owned(),
            method_name: "run".to_owned(),
            expected_sandbox_id: Some(EXPECTED_SANDBOX_ID.to_owned()),
            environment: BTreeMap::from([("RUST_BACKTRACE".to_owned(), "0".to_owned())]),
        }))
    }
}

struct EmptyAssessmentContext;

impl ApprovedAssessmentContextSource for EmptyAssessmentContext {
    fn resolve(
        &self,
        _run_id: &str,
        _attempt_id: &str,
        _source: &lookback_tauri_lib::coding_agent::assessment::AllowedEvidenceSource,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        Ok(None)
    }
}

struct FinalizationExecution {
    snapshot: AgentRunSnapshot,
    observation: HostFinalizationObservation,
    typed: tokio::sync::Mutex<TypedFinalizationEvidence>,
    cancellation_observation: std::sync::Mutex<Option<CancellationObservation>>,
}

#[async_trait]
impl ExecutionAdapter for FinalizationExecution {
    async fn preflight(
        &self,
        _snapshot: &AgentRunSnapshot,
    ) -> Result<RuntimeObservation, ExecutionResult> {
        Ok(finalization_runtime_observation(&self.snapshot))
    }

    async fn prepare_workspace(
        &self,
        request: &WorkspaceRequest,
    ) -> Result<WorkspaceReceipt, ExecutionResult> {
        Ok(finalization_workspace_receipt(request))
    }

    async fn reconcile_workspace(
        &self,
        request: &WorkspaceRequest,
    ) -> Result<OperationReconciliation<WorkspaceReceipt>, ExecutionResult> {
        Ok(OperationReconciliation::Accepted(
            finalization_workspace_receipt(request),
        ))
    }

    async fn start_attempt(
        &self,
        _snapshot: &AgentRunSnapshot,
        intent: &ImmutableExecutionIntent,
        _workspace: &WorkspaceReceipt,
        _prompt: Option<Vec<u8>>,
    ) -> Result<StartReceipt, ExecutionResult> {
        let bytes = serde_json::to_vec(intent).map_err(|_| ExecutionResult::Conflict)?;
        Ok(StartReceipt {
            identity: finalization_identity(&self.snapshot),
            execution_intent_sha256: sha256_hex(&bytes),
            method_name: intent.method_name.clone(),
            image_digest: intent.image_digest.clone(),
            prompt_delivery: PromptDelivery::Accepted,
        })
    }

    async fn reconcile_start(
        &self,
        _snapshot: &AgentRunSnapshot,
        _intent: &ImmutableExecutionIntent,
    ) -> Result<OperationReconciliation<StartReceipt>, ExecutionResult> {
        Ok(OperationReconciliation::Unknown(None))
    }

    async fn events(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<ExecutionEventStream, ExecutionResult> {
        Ok(Box::pin(futures::stream::iter([
            Ok(ExecutionEvent::Output {
                stream: "stdout".to_owned(),
                data: b"completed".to_vec(),
            }),
            Ok(ExecutionEvent::Exit { exit_code: 0 }),
        ])))
    }

    async fn terminal_control(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<Arc<dyn TerminalControl>, ExecutionResult> {
        Ok(Arc::new(FinalizationTerminalControl))
    }

    async fn finalize_one_shot(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<HostFinalizationObservation, ExecutionResult> {
        Ok(self.observation.clone())
    }

    async fn finalize_interactive(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<InteractiveFinishObservation, ExecutionResult> {
        Ok(InteractiveFinishObservation {
            invocation_kind: self.observation.invocation_kind,
            cli_exit: self.observation.cli_exit.clone(),
            shell_observation: self.observation.shell_observation.clone(),
            child_writers: self.observation.child_writers,
            workspace_freeze: self.observation.workspace_freeze.clone(),
            report: self.observation.report.clone(),
            validation: self.observation.validation.clone(),
            acceptance_conditions_met: self.observation.acceptance_conditions_met,
        })
    }

    async fn finalization_evidence(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<TypedFinalizationEvidence, ExecutionResult> {
        if identity == &finalization_identity(&self.snapshot) {
            Ok(self.typed.lock().await.clone())
        } else {
            Err(ExecutionResult::Conflict)
        }
    }

    async fn reconcile_interactive_finish(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<InteractiveFinishObservation>, ExecutionResult> {
        Ok(OperationReconciliation::Unknown(None))
    }

    async fn cancel(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<CancellationObservation>, ExecutionResult> {
        Ok(self
            .cancellation_observation
            .lock()
            .unwrap()
            .clone()
            .map_or(OperationReconciliation::Unknown(None), |observation| {
                OperationReconciliation::Accepted(observation)
            }))
    }

    async fn reconcile_cancel(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<CancellationObservation>, ExecutionResult> {
        Ok(self
            .cancellation_observation
            .lock()
            .unwrap()
            .clone()
            .map_or(OperationReconciliation::Unknown(None), |observation| {
                OperationReconciliation::Accepted(observation)
            }))
    }

    async fn heartbeat(&self, _identity: &ExecutionIdentity) -> Result<(), ExecutionResult> {
        Ok(())
    }
}

struct FinalizationTerminalControl;

#[async_trait]
impl TerminalControl for FinalizationTerminalControl {
    async fn send_input(
        &self,
        _bytes: Vec<u8>,
    ) -> Result<lookback_tauri_lib::coding_agent::InputDelivery, ExecutionResult> {
        Ok(lookback_tauri_lib::coding_agent::InputDelivery::Accepted)
    }

    async fn finish_input(
        &self,
    ) -> Result<lookback_tauri_lib::coding_agent::InputDelivery, ExecutionResult> {
        Ok(lookback_tauri_lib::coding_agent::InputDelivery::Accepted)
    }
}

fn finalization_snapshot() -> AgentRunSnapshot {
    let repository = RepositoryIdentity::new("forge.example", "org", "repo").unwrap();
    let image_digest = format!("sha256:{}", "a".repeat(64));
    AgentRunSnapshot {
        run_id: RunId::new("run-1").unwrap(),
        issue_id: "issue-1".to_owned(),
        issue_revision: 1,
        execution_mode: ExecutionMode::OneShot,
        purpose: RunPurpose::Implementation,
        repository: ResolvedRepository {
            identity: repository.clone(),
            forge: ForgeKind::Gitea,
            api_base_url: "https://forge.example/api/v1".to_owned(),
            clone_url: "https://forge.example/org/repo.git".to_owned(),
            read_credential_ref: None,
            publication_credential_ref: None,
            provenance: RepositoryProvenance {
                source_labels: vec!["repo:forge.example/org/repo".to_owned()],
                selection: RepositorySelection::SourceLabel,
                approved_by: None,
                approved_at_ms: None,
            },
        },
        profile: AgentProfileSnapshot {
            profile_id: "test-coder".to_owned(),
            version: 1,
            display_name: "Test coder".to_owned(),
            image_ref: format!("registry.example/coder@{image_digest}"),
            image_digest,
            command: vec!["opencode".to_owned(), "run".to_owned()],
            container_home: "/home/agent".to_owned(),
            managed_home_overlay: None,
            runtime_environment: Default::default(),
            network_access: Default::default(),
            resources: SandboxResources {
                cpu_millis: 1000,
                memory_mib: 1024,
                root_disk_mib: 4096,
                max_parallel_runs: 1,
                use_static: false,
            },
            validated_host_limits: HostResourceLimits {
                cpu_millis: 8000,
                memory_mib: 16384,
                root_disk_mib: 65536,
                max_parallel_runs: 4,
            },
            canonical_mount_allowed_roots: vec!["/tmp/approved".into()],
            persistent_mounts: Vec::new(),
            allowed_repositories: vec![repository],
            credential_reference: None,
        },
        base_commit: "a".repeat(40),
        workspace_branch: "agent/run-1".to_owned(),
        semantic_judge_enabled: true,
        auto_done: false,
        approval: Some(RunApproval {
            approved_by: "test-reviewer".to_owned(),
            approved_at_ms: 1,
            auto_done_approved: false,
            settings_version: Some(1),
            applicable_modes: vec![ExecutionMode::OneShot],
        }),
        allowed_operations: vec![AllowedOperation::Verify],
        publication_plan: PublicationPlan::NoPublication,
        workflow: WorkflowDefinitionSnapshot {
            definition_hash: "c".repeat(64),
            semantic_judge_worker_digest: None,
            template_id: Some("coding-agent".to_owned()),
            materialized_definition_hash: Some("d".repeat(64)),
            registered_worker_id: Some(WORKER_ID.to_string()),
            workflow_execution_id: Some("workflow-1".to_owned()),
            parent_job_id: Some("parent-1".to_owned()),
            child_sandbox_job_id: Some("child-1".to_owned()),
        },
        attempt_ids: vec!["attempt-1".to_owned()],
    }
}

fn finalization_contract() -> ApprovalAssessmentContract {
    ApprovalAssessmentContract::from_issue_acceptance(
        RunPurpose::Implementation,
        vec!["The approved behavior is present.".to_owned()],
        vec![ApprovalCriterionPolicy {
            required: false,
            method: CriterionEvaluationMethod::Deterministic,
        }],
        vec![ApprovalValidationDraft {
            required: true,
            command: Some(ApprovalAssessmentCommand {
                argv: vec![
                    "cargo".to_owned(),
                    "test".to_owned(),
                    "--workspace".to_owned(),
                ],
                working_directory: "/workspace/repository".to_owned(),
                permissions: vec![ValidationPermission::ReadWorkspace],
                profile_digest: "d".repeat(64),
                timeout_ms: 60_000,
            }),
        }],
        None,
        None,
        AssessmentInputScope {
            allowed_fields: Vec::new(),
            allowed_sources: Vec::new(),
            max_input_bytes: 4096,
        },
    )
    .unwrap()
}

fn finalization_identity(snapshot: &AgentRunSnapshot) -> ExecutionIdentity {
    ExecutionIdentity {
        run_id: snapshot.run_id.to_string(),
        attempt_id: "attempt-1".to_owned(),
        workflow_execution_id: "workflow-1".to_owned(),
        parent_job_id: "parent-1".to_owned(),
        child_sandbox_job_id: "child-1".to_owned(),
        worker_id: WORKER_ID,
        sandbox_job_id: JOB_ID.to_string(),
    }
}

fn finalization_workspace_receipt(request: &WorkspaceRequest) -> WorkspaceReceipt {
    WorkspaceReceipt {
        run_id: request.snapshot.run_id.to_string(),
        attempt_id: request.attempt_id.clone(),
        workspace_token: "workspace-test".to_owned(),
        repository_host: request.snapshot.repository.identity.host.clone(),
        repository_owner: request.snapshot.repository.identity.organization.clone(),
        repository_name: request.snapshot.repository.identity.repository.clone(),
        base_commit: request.snapshot.base_commit.clone(),
        branch: request.snapshot.workspace_branch.clone(),
    }
}

fn finalization_runtime_observation(snapshot: &AgentRunSnapshot) -> RuntimeObservation {
    let interactive = snapshot.execution_mode == ExecutionMode::Interactive;
    let mut helper_features = BTreeSet::from([
        "prepare".to_owned(),
        "frozen_commit_import".to_owned(),
        "host_process_observation".to_owned(),
        "host_writer_quiescence".to_owned(),
        "host_workspace_freeze".to_owned(),
        "binary_output_capture".to_owned(),
        "protected_remote_auth_forwarding_verified".to_owned(),
    ]);
    if interactive {
        helper_features.insert("interactive_finish".to_owned());
    }
    RuntimeObservation {
        runtime_instance_id: "runtime-test".to_owned(),
        health_observation_id: "health-test".to_owned(),
        worker_name: "agent-worker".to_owned(),
        worker_id: WORKER_ID,
        runner_name: if interactive { "SANDBOX" } else { "WORKFLOW" }.to_owned(),
        method_name: if interactive {
            "run_with_client".to_owned()
        } else {
            "run".to_owned()
        },
        arguments_schema_sha256: "a".repeat(64),
        result_schema_sha256: "b".repeat(64),
        response_mode: "DIRECT".to_owned(),
        output_is_streaming: true,
        accepts_client_stream: interactive,
        image_digest: snapshot.profile.image_digest.clone(),
        admitted_resources: snapshot.profile.resources,
        host_limits: snapshot.profile.validated_host_limits,
        helper_version: "test-helper".to_owned(),
        agent_cli_version: if interactive { "1.18.3" } else { "2.0.24" }.to_owned(),
        helper_protocol_version: 1,
        helper_features,
        workflow_execution_id: "workflow-1".to_owned(),
        parent_job_id: "parent-1".to_owned(),
        child_sandbox_job_id: "child-1".to_owned(),
    }
}

fn finalization_report() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "run_id": "run-1",
        "attempt_id": "attempt-1",
        "task_kind": "implementation",
        "status": "partial",
        "summary": "The host verified the approved validation receipt.",
        "acceptance": [{
            "criterion_id": "AC-1",
            "assessment": "unknown",
            "evidence": ["report:attempt-1"]
        }],
        "validation": [{
            "validation_id": "V-1",
            "claimed_result": "not_run",
            "evidence": [],
            "reason": "Guest report text is not a host-issued validation receipt."
        }],
        "investigation": null,
        "implementation": {
            "change_summary": "Validation evidence integration test.",
            "changed_files": [],
            "implementation_notes": []
        },
        "unresolved": []
    }))
    .unwrap()
}

async fn fake_native_executor(
    root: &std::path::Path,
    artifact: &ResolvedArtifact,
) -> NativeExecutorClient {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let workspace_root = create_private_dir(root, "executor-workspaces");
    let publisher_root = create_private_dir(root, "executor-publishers");
    let state_root = create_private_dir(root, "executor-state");
    let credential_root = create_private_dir(root, "executor-credentials");
    let package = create_private_dir(root, "executor-package");
    let executable = package.join("executor-fixture");
    let capabilities = serde_json::to_string(&CapabilityDocument::current()).unwrap();
    let response = serde_json::json!({
        "protocol_version": PROTOCOL_VERSION,
        "status": "ok",
        "result": { "resolved": artifact }
    })
    .to_string();
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--capabilities\" ]; then\n  printf '%s\\n' {}\nelse\n  cat >/dev/null\n  printf '%s\\n' {}\nfi\n",
        shell_quote(&capabilities),
        shell_quote(&response),
    );
    fs::write(&executable, script).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let config_path = root.join("executor-trusted-config.json");
    let config = TrustedConfig {
        protocol_version: PROTOCOL_VERSION,
        expected_executor_version: EXECUTOR_VERSION.to_owned(),
        workspace_root,
        publisher_root,
        state_root,
        credential_root,
        max_import_bytes: 1024 * 1024,
        repositories: vec![RepositoryConfig {
            id: artifact.repository.clone(),
            clone_url: "https://forge.example/org/repo.git".to_owned(),
            ssh_directory: None,
            allowed_base_branches: vec!["main".to_owned()],
            read_credential_source: None,
            publish_credential_source: None,
            forge: None,
        }],
        credential_sources: Vec::new(),
    };
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
    let executable_digest = sha256_hex(&fs::read(&executable).unwrap());
    NativeExecutorClient::connect(executable, config_path, executable_digest)
        .await
        .unwrap()
}

fn create_private_dir(parent: &std::path::Path, name: &str) -> std::path::PathBuf {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let path = parent.join(name);
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}
