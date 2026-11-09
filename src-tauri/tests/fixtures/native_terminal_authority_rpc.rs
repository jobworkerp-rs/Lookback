use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use crate::grpc::proto::issue_tracker::{manager::v1 as manager, v1};
use tonic::{
    body::Body,
    codegen::{Service, http},
    server::{NamedService, UnaryService},
};

type RpcFuture = Pin<Box<dyn Future<Output = Result<http::Response<Body>, Infallible>> + Send>>;
type UnaryFuture<T> =
    Pin<Box<dyn Future<Output = Result<tonic::Response<T>, tonic::Status>> + Send>>;

#[derive(Clone, Debug, Default)]
pub struct RpcObservations {
    pub contract_context: Option<manager::ManagerReadContext>,
    pub get_issue_args: Option<manager::GetIssueWithSyncArgs>,
    pub contract_probe_count: usize,
    pub get_issue_count: usize,
}

#[derive(Default)]
struct SharedState {
    issue: v1::Issue,
    observations: Mutex<RpcObservations>,
}

pub struct NativeAuthorityRpcFixture {
    endpoint: String,
    state: Arc<SharedState>,
    server: Option<tokio::task::JoinHandle<()>>,
}

impl NativeAuthorityRpcFixture {
    pub async fn start(issue: v1::Issue) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test Issue Tracker listener");
        let address = listener.local_addr().expect("read listener address");
        let state = Arc::new(SharedState {
            issue,
            observations: Mutex::new(RpcObservations::default()),
        });
        let incoming = futures::stream::unfold(listener, |listener| async move {
            let accepted = listener.accept().await.map(|(stream, _)| stream);
            Some((accepted, listener))
        });
        let server = tonic::transport::Server::builder()
            .add_service(ManagerInfoRpc(Arc::clone(&state)))
            .add_service(ManagerConsumerRpc(Arc::clone(&state)))
            .serve_with_incoming(incoming);
        let server = tokio::spawn(async move {
            let _ = server.await;
        });
        Self {
            endpoint: format!("http://{address}"),
            state,
            server: Some(server),
        }
    }

    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    pub fn observations(&self) -> RpcObservations {
        self.state.observations.lock().unwrap().clone()
    }

    pub async fn stop(mut self) {
        let server = self.server.take().expect("test RPC server is running");
        server.abort();
        let _ = server.await;
    }
}

impl Drop for NativeAuthorityRpcFixture {
    fn drop(&mut self) {
        if let Some(server) = &self.server {
            server.abort();
        }
    }
}

#[derive(Clone)]
struct ManagerInfoRpc(Arc<SharedState>);

impl Service<http::Request<Body>> for ManagerInfoRpc {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = RpcFuture;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        if request.uri().path() != "/issue_tracker.rpc.v1.ManagerInfoService/GetContractInfo" {
            return unimplemented();
        }
        let service = GetContractInfoRpc(Arc::clone(&self.0));
        Box::pin(async move {
            let response = tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                .unary(service, request)
                .await;
            Ok(response)
        })
    }
}

impl NamedService for ManagerInfoRpc {
    const NAME: &'static str = "issue_tracker.rpc.v1.ManagerInfoService";
}

struct GetContractInfoRpc(Arc<SharedState>);

impl UnaryService<manager::GetContractInfoArgs> for GetContractInfoRpc {
    type Response = manager::GetContractInfoResult;
    type Future = UnaryFuture<Self::Response>;

    fn call(&mut self, request: tonic::Request<manager::GetContractInfoArgs>) -> Self::Future {
        let mut observations = self.0.observations.lock().unwrap();
        observations.contract_context = request.into_inner().context;
        observations.contract_probe_count += 1;
        drop(observations);
        Box::pin(async {
            Ok(tonic::Response::new(manager::GetContractInfoResult {
                outcome: Some(manager::get_contract_info_result::Outcome::Success(
                    manager::ContractInfo {
                        contract_version: "issue-tracker-manager/v14".into(),
                        contract_fingerprint: "authority-contract-fingerprint".into(),
                        schema_fingerprint: "authority-schema-fingerprint".into(),
                        store_identities: vec![
                            manager::StoreIdentity {
                                role: manager::StoreRole::Manager as i32,
                                data_root_id: "native-authority-test-root".into(),
                                store_id: "manager-store".into(),
                                schema_fingerprint: "authority-schema-fingerprint".into(),
                                store_pair_id: "authority-store-pair".into(),
                                contract_fingerprint: "authority-contract-fingerprint".into(),
                                ..Default::default()
                            },
                            manager::StoreIdentity {
                                role: manager::StoreRole::Taskchampion as i32,
                                data_root_id: "native-authority-test-root".into(),
                                store_id: "backend-store".into(),
                                schema_fingerprint: "backend-schema-fingerprint".into(),
                                store_pair_id: "authority-store-pair".into(),
                                contract_fingerprint: "backend-contract-fingerprint".into(),
                                ..Default::default()
                            },
                        ],
                        features: vec![
                            manager::ManagerFeature::DurableOperations as i32,
                            manager::ManagerFeature::IssueAdminActions as i32,
                            manager::ManagerFeature::OwnershipRecovery as i32,
                            manager::ManagerFeature::OwnershipDiagnostics as i32,
                            manager::ManagerFeature::IssueOperationResolution as i32,
                        ],
                        health: manager::HealthStatus::Healthy as i32,
                        activation: manager::ActivationStatus::Ready as i32,
                        root_operation_history_max_window: Some(manager::TimeSpan { millis: 1 }),
                        ..Default::default()
                    },
                )),
            }))
        })
    }
}

#[derive(Clone)]
struct ManagerConsumerRpc(Arc<SharedState>);

impl Service<http::Request<Body>> for ManagerConsumerRpc {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = RpcFuture;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        if request.uri().path() != "/issue_tracker.rpc.v1.ManagerConsumerService/GetIssueWithSync" {
            return unimplemented();
        }
        let service = GetIssueWithSyncRpc(Arc::clone(&self.0));
        Box::pin(async move {
            let response = tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                .unary(service, request)
                .await;
            Ok(response)
        })
    }
}

impl NamedService for ManagerConsumerRpc {
    const NAME: &'static str = "issue_tracker.rpc.v1.ManagerConsumerService";
}

struct GetIssueWithSyncRpc(Arc<SharedState>);

impl UnaryService<manager::GetIssueWithSyncArgs> for GetIssueWithSyncRpc {
    type Response = manager::GetIssueWithSyncResult;
    type Future = UnaryFuture<Self::Response>;

    fn call(&mut self, request: tonic::Request<manager::GetIssueWithSyncArgs>) -> Self::Future {
        let args = request.into_inner();
        let mut observations = self.0.observations.lock().unwrap();
        observations.get_issue_args = Some(args);
        observations.get_issue_count += 1;
        drop(observations);
        let issue = self.0.issue.clone();
        Box::pin(async move {
            Ok(tonic::Response::new(manager::GetIssueWithSyncResult {
                outcome: Some(manager::get_issue_with_sync_result::Outcome::Success(
                    manager::IssueWithSync {
                        issue: Some(issue),
                        ..Default::default()
                    },
                )),
            }))
        })
    }
}

fn unimplemented() -> RpcFuture {
    Box::pin(async {
        let mut response = http::Response::new(Body::default());
        let headers = response.headers_mut();
        headers.insert(
            tonic::Status::GRPC_STATUS,
            (tonic::Code::Unimplemented as i32).into(),
        );
        headers.insert(
            http::header::CONTENT_TYPE,
            tonic::metadata::GRPC_CONTENT_TYPE,
        );
        Ok(response)
    })
}
