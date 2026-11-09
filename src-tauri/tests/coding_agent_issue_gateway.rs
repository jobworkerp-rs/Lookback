use lookback_tauri_lib::{
    coding_agent::issue_gateway::{AutomaticIssueGateway, ClaimedAutomaticIssue},
    grpc::proto::issue_tracker::{rpc::v1 as rpc, v1 as issue},
};
use std::sync::{Arc, Mutex};
use tonic::{Request, Response, Status};

#[derive(Clone)]
struct GatewayFixture {
    claim: Arc<Mutex<rpc::IssueGatewayClaimResponse>>,
    claims: Arc<Mutex<Vec<rpc::IssueGatewayClaimRequest>>>,
    completions: Arc<Mutex<Vec<rpc::IssueGatewayCompleteExecutionToAwaitingReviewRequest>>>,
    completion_failure: Arc<Mutex<bool>>,
    completion_mismatch: Arc<Mutex<bool>>,
    heartbeats: Arc<Mutex<Vec<rpc::IssueGatewayHeartbeatRequest>>>,
    heartbeat_failure: Arc<Mutex<bool>>,
    heartbeat_response: Arc<Mutex<Option<rpc::IssueGatewayMutationResponse>>>,
    heartbeat_gate: Arc<Mutex<Option<Arc<tokio::sync::Notify>>>>,
}

#[tokio::test]
async fn claimed_execution_renews_its_lease_and_saves_an_admitted_result_once() {
    let fixture = GatewayFixture::new();
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    let execution = ClaimedAutomaticIssue::new(gateway, claim);
    execution.renewal_guard().accept();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while fixture.heartbeats.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        while execution.owned_revision().await.unwrap().revision() == 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    execution
        .admit_result("run-1", &"a".repeat(40), b"{}")
        .await
        .unwrap();
    assert!(fixture.completions.lock().unwrap().is_empty());
    let receipt = execution.save_result().await.unwrap();
    assert!(receipt.lease_closed);
    assert!(receipt.revision >= 10);
    assert!(execution.save_result().await.is_err());
    assert!(
        execution
            .admit_result("run-1", &"a".repeat(40), b"{}")
            .await
            .is_err()
    );
    assert_eq!(fixture.completions.lock().unwrap().len(), 1);
    task.abort();
}

#[tokio::test]
async fn cancellation_prevents_new_result_admission_but_preserves_an_admitted_drain() {
    for admitted in [false, true] {
        let fixture = GatewayFixture::new();
        let (gateway, task) = server(fixture.clone()).await;
        let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
        let execution = ClaimedAutomaticIssue::new(gateway, claim);
        if admitted {
            execution
                .admit_result("run-1", &"a".repeat(40), b"{}")
                .await
                .unwrap();
        }
        execution.cancel().await;
        assert!(
            execution
                .admit_result("run-1", &"a".repeat(40), b"{}")
                .await
                .is_err()
        );
        assert_eq!(execution.save_result().await.is_ok(), admitted);
        assert_eq!(
            fixture.completions.lock().unwrap().len(),
            usize::from(admitted)
        );
        task.abort();
    }
}

#[tokio::test]
async fn lost_result_response_cannot_be_saved_again_by_the_execution_owner() {
    let fixture = GatewayFixture::new();
    *fixture.completion_failure.lock().unwrap() = true;
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    let execution = ClaimedAutomaticIssue::new(gateway, claim);
    execution
        .admit_result("run-1", &"a".repeat(40), b"{}")
        .await
        .unwrap();
    assert!(execution.save_result().await.is_err());
    assert!(execution.save_result().await.is_err());
    assert_eq!(fixture.completions.lock().unwrap().len(), 1);
    task.abort();
}

#[tokio::test]
async fn lease_cancellation_does_not_wait_for_an_inflight_heartbeat_response() {
    let fixture = GatewayFixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    *fixture.heartbeat_gate.lock().unwrap() = Some(gate.clone());
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    let execution = Arc::new(ClaimedAutomaticIssue::new(gateway, claim));
    let heartbeat = tokio::spawn({
        let execution = execution.clone();
        async move { execution.heartbeat().await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fixture.heartbeats.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_millis(500), execution.cancel())
        .await
        .unwrap();
    assert!(
        execution
            .admit_result("run-1", &"a".repeat(40), b"{}")
            .await
            .is_err()
    );
    gate.notify_one();
    heartbeat.await.unwrap().unwrap();
    assert!(execution.heartbeat().await.is_err());
    task.abort();
}

#[tokio::test]
async fn dropping_the_execution_owner_stops_background_lease_renewal() {
    let fixture = GatewayFixture::new();
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    let execution = ClaimedAutomaticIssue::new(gateway, claim);
    drop(execution);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert!(fixture.heartbeats.lock().unwrap().is_empty());
    task.abort();
}

#[tokio::test]
async fn abandoning_start_admission_stops_renewal_even_while_the_claim_is_cached() {
    let fixture = GatewayFixture::new();
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    let execution = ClaimedAutomaticIssue::new(gateway, claim);
    let admission = execution.renewal_guard();
    drop(admission);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert!(fixture.heartbeats.lock().unwrap().is_empty());
    assert!(fixture.completions.lock().unwrap().is_empty());
    drop(execution);
    task.abort();
}
impl GatewayFixture {
    fn new() -> Self {
        Self {
            claim: Arc::new(Mutex::new(rpc::IssueGatewayClaimResponse {
                outcome: Some(rpc::issue_gateway_claim_response::Outcome::Success(
                    rpc::IssueGatewayClaimed {
                        issue_id: "issue-1".into(),
                        capability_id: "private-capability".into(),
                        revision: 8,
                    },
                )),
            })),
            claims: Arc::new(Mutex::new(Vec::new())),
            completions: Arc::new(Mutex::new(Vec::new())),
            completion_failure: Arc::new(Mutex::new(false)),
            completion_mismatch: Arc::new(Mutex::new(false)),
            heartbeats: Arc::new(Mutex::new(Vec::new())),
            heartbeat_failure: Arc::new(Mutex::new(false)),
            heartbeat_response: Arc::new(Mutex::new(None)),
            heartbeat_gate: Arc::new(Mutex::new(None)),
        }
    }
}
type RpcFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;
impl tonic::server::NamedService for GatewayFixture {
    const NAME: &'static str = "issue_tracker.rpc.v1.IssueGateway";
}
impl tonic::codegen::Service<tonic::codegen::http::Request<tonic::body::Body>> for GatewayFixture {
    type Response = tonic::codegen::http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = RpcFuture<Result<Self::Response, Self::Error>>;
    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: tonic::codegen::http::Request<tonic::body::Body>) -> Self::Future {
        let state = self.clone();
        let path = request.uri().path().to_owned();
        Box::pin(async move {
            let response = match path.as_str() {
                "/issue_tracker.rpc.v1.IssueGateway/Claim" => {
                    tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                        .unary(ClaimRpc(state), request)
                        .await
                }
                "/issue_tracker.rpc.v1.IssueGateway/Heartbeat" => {
                    tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                        .unary(HeartbeatRpc(state), request)
                        .await
                }
                "/issue_tracker.rpc.v1.IssueGateway/CompleteExecutionToAwaitingReview" => {
                    tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                        .unary(CompleteRpc(state), request)
                        .await
                }
                _ => tonic::codegen::http::Response::builder()
                    .status(200)
                    .header("content-type", "application/grpc")
                    .header("grpc-status", "12")
                    .body(tonic::body::Body::empty())
                    .unwrap(),
            };
            Ok(response)
        })
    }
}
struct HeartbeatRpc(GatewayFixture);
impl tonic::server::UnaryService<rpc::IssueGatewayHeartbeatRequest> for HeartbeatRpc {
    type Response = rpc::IssueGatewayMutationResponse;
    type Future = RpcFuture<Result<Response<Self::Response>, Status>>;
    fn call(&mut self, request: Request<rpc::IssueGatewayHeartbeatRequest>) -> Self::Future {
        let request = request.into_inner();
        self.0.heartbeats.lock().unwrap().push(request.clone());
        let failure = *self.0.heartbeat_failure.lock().unwrap();
        let response = self.0.heartbeat_response.lock().unwrap().clone();
        let gate = self.0.heartbeat_gate.lock().unwrap().clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.notified().await;
            }
            if failure {
                return Err(Status::unavailable("response lost"));
            }
            if let Some(response) = response {
                return Ok(Response::new(response));
            }
            Ok(Response::new(rpc::IssueGatewayMutationResponse {
                outcome: Some(rpc::issue_gateway_mutation_response::Outcome::Success(
                    rpc::IssueGatewayMutation {
                        issue_id: request.issue_id,
                        revision: request.expected_revision.unwrap() + 1,
                    },
                )),
            }))
        })
    }
}
struct ClaimRpc(GatewayFixture);
impl tonic::server::UnaryService<rpc::IssueGatewayClaimRequest> for ClaimRpc {
    type Response = rpc::IssueGatewayClaimResponse;
    type Future = RpcFuture<Result<Response<Self::Response>, Status>>;
    fn call(&mut self, request: Request<rpc::IssueGatewayClaimRequest>) -> Self::Future {
        self.0.claims.lock().unwrap().push(request.into_inner());
        let response = self.0.claim.lock().unwrap().clone();
        Box::pin(async move { Ok(Response::new(response)) })
    }
}
struct CompleteRpc(GatewayFixture);
impl tonic::server::UnaryService<rpc::IssueGatewayCompleteExecutionToAwaitingReviewRequest>
    for CompleteRpc
{
    type Response = rpc::IssueGatewayCompleteExecutionToAwaitingReviewResponse;
    type Future = RpcFuture<Result<Response<Self::Response>, Status>>;
    fn call(
        &mut self,
        request: Request<rpc::IssueGatewayCompleteExecutionToAwaitingReviewRequest>,
    ) -> Self::Future {
        let request = request.into_inner();
        self.0.completions.lock().unwrap().push(request.clone());
        let failure = *self.0.completion_failure.lock().unwrap();
        let mismatch = *self.0.completion_mismatch.lock().unwrap();
        let completion = request.completion.unwrap();
        Box::pin(async move {
            if failure {
                return Err(Status::unavailable("response lost"));
            }
            let issue_id = if mismatch {
                "another-issue".into()
            } else {
                request.issue_id
            };
            Ok(Response::new(rpc::IssueGatewayCompleteExecutionToAwaitingReviewResponse { outcome: Some(rpc::issue_gateway_complete_execution_to_awaiting_review_response::Outcome::Success(rpc::IssueGatewayAwaitingReviewMutation { issue_id, revision: request.expected_revision.unwrap()+1, run_id: completion.run_id, full_commit_id: completion.full_commit_id, evidence_references: completion.evidence_references, lease_closed: true })) }))
        })
    }
}
async fn server(fixture: GatewayFixture) -> (AutomaticIssueGateway, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(fixture)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    (AutomaticIssueGateway::new(endpoint).unwrap(), task)
}
#[tokio::test]
async fn claim_is_bound_to_the_run_and_revision_and_cannot_be_dispatched_twice() {
    let fixture = GatewayFixture::new();
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    assert_eq!(claim.issue_id(), "issue-1");
    assert_eq!(claim.revision(), 8);
    assert!(!format!("{claim:?}").contains("private-capability"));
    assert!(gateway.claim("run-1", "issue-1", 7).await.is_err());
    assert!(gateway.claim("run-1", "issue-2", 7).await.is_err());
    let requests = fixture.claims.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].issue_id, "issue-1");
    assert_eq!(requests[0].expected_revision, Some(7));
    assert!(!requests[0].idempotency_key.is_empty());
    task.abort();
}
#[tokio::test]
async fn missing_or_mismatched_claim_responses_remain_unconfirmed_without_reclaim() {
    for response in [
        None,
        Some(rpc::IssueGatewayClaimed {
            issue_id: "another-issue".into(),
            capability_id: "private-capability".into(),
            revision: 8,
        }),
        Some(rpc::IssueGatewayClaimed {
            issue_id: "issue-1".into(),
            capability_id: String::new(),
            revision: 8,
        }),
        Some(rpc::IssueGatewayClaimed {
            issue_id: "issue-1".into(),
            capability_id: "private-capability".into(),
            revision: 7,
        }),
    ] {
        let fixture = GatewayFixture::new();
        fixture.claim.lock().unwrap().outcome =
            response.map(rpc::issue_gateway_claim_response::Outcome::Success);
        let (gateway, task) = server(fixture.clone()).await;
        let error = gateway.claim("run-1", "issue-1", 7).await.unwrap_err();
        assert!(!format!("{error:?}").contains("private-capability"));
        assert!(gateway.claim("run-1", "issue-1", 7).await.is_err());
        assert_eq!(fixture.claims.lock().unwrap().len(), 1);
        task.abort();
    }
}
#[tokio::test]
async fn actual_report_and_commit_are_saved_through_the_dedicated_review_path() {
    let fixture = GatewayFixture::new();
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    let report = serde_json::json!({"findings":"調査結果\n引用付き", "status":"completed"});
    let bytes = serde_json::to_vec(&report).unwrap();
    let result = gateway
        .complete_to_review(&claim, "run-1", &"a".repeat(40), &bytes)
        .await
        .unwrap();
    assert_eq!(result.revision, 9);
    assert!(result.lease_closed);
    let requests = fixture.completions.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].capability_id, "private-capability");
    assert_eq!(requests[0].expected_revision, Some(8));
    let saved = requests[0].completion.as_ref().unwrap();
    assert_eq!(saved.run_id, "run-1");
    assert_eq!(saved.full_commit_id, "a".repeat(40));
    let Some(issue::completion_report::Result::Verified(saved_report)) =
        saved.report.as_ref().unwrap().result.as_ref()
    else {
        panic!("saved report evidence missing")
    };
    let reconstructed = saved_report
        .evidence
        .iter()
        .map(|entry| {
            assert_eq!(
                entry.kind,
                issue::CompletionEvidenceKind::OtherEvidence as i32
            );
            entry
                .summary
                .split_once(':')
                .filter(|(index, _)| index.bytes().all(|byte| byte.is_ascii_digit()))
                .map_or(entry.summary.as_str(), |(_, part)| part)
        })
        .collect::<String>();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reconstructed).unwrap(),
        report
    );
    task.abort();
}

#[tokio::test]
async fn a_claim_cannot_be_completed_by_another_execution_gateway() {
    let fixture = GatewayFixture::new();
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    // A second execution must not consume the first execution's claim.
    let endpoint = fixture.completions.clone();
    let (other_gateway, other_task) = server(fixture.clone()).await;
    assert!(other_gateway.owned_revision(&claim).await.is_err());
    assert!(
        other_gateway
            .complete_to_review(&claim, "run-1", &"a".repeat(40), b"{}")
            .await
            .is_err()
    );
    assert!(endpoint.lock().unwrap().is_empty());
    assert!(
        gateway
            .complete_to_review(&claim, "run-1", &"a".repeat(40), b"{}")
            .await
            .is_ok()
    );
    task.abort();
    other_task.abort();
}

#[tokio::test]
async fn long_unicode_and_whitespace_reports_are_preserved_without_invalid_chunks() {
    for finding in [
        "日本語".repeat(5000),
        " ".repeat(16000),
        "\u{0085}".repeat(5000),
    ] {
        let fixture = GatewayFixture::new();
        let (gateway, task) = server(fixture.clone()).await;
        let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
        let report = serde_json::json!({"finding":finding});
        gateway
            .complete_to_review(
                &claim,
                "run-1",
                &"a".repeat(40),
                &serde_json::to_vec(&report).unwrap(),
            )
            .await
            .unwrap();
        let requests = fixture.completions.lock().unwrap();
        let Some(issue::completion_report::Result::Verified(saved)) = requests[0]
            .completion
            .as_ref()
            .unwrap()
            .report
            .as_ref()
            .unwrap()
            .result
            .as_ref()
        else {
            panic!("report missing")
        };
        assert!(saved.evidence.len() > 1);
        let reconstructed = saved
            .evidence
            .iter()
            .map(|entry| {
                assert!(entry.summary.len() <= 4096);
                assert!(!entry.summary.trim().is_empty());
                assert!(!entry.summary.chars().any(char::is_control));
                entry
                    .summary
                    .split_once(':')
                    .filter(|(index, _)| index.bytes().all(|byte| byte.is_ascii_digit()))
                    .map_or(entry.summary.as_str(), |(_, part)| part)
            })
            .collect::<String>();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reconstructed).unwrap(),
            report
        );
        task.abort();
    }
}

#[tokio::test]
async fn invalid_reports_do_not_consume_the_claim_or_send_a_completion() {
    let fixture = GatewayFixture::new();
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    for bytes in [
        b"[]".to_vec(),
        b"invalid".to_vec(),
        vec![b' '; lookback_tauri_lib::coding_agent::MAX_REPORT_BYTES + 1],
        serde_json::to_vec(&serde_json::json!({"finding":"\u{007f}".repeat(700000)})).unwrap(),
    ] {
        assert!(
            gateway
                .complete_to_review(&claim, "run-1", &"a".repeat(40), &bytes)
                .await
                .is_err()
        );
    }
    assert!(fixture.completions.lock().unwrap().is_empty());
    gateway
        .complete_to_review(&claim, "run-1", &"a".repeat(40), b"{}")
        .await
        .unwrap();
    assert_eq!(fixture.completions.lock().unwrap().len(), 1);
    task.abort();
}

#[tokio::test]
async fn completion_is_never_repeated_after_success_lost_response_or_mismatched_ack() {
    for (failure, mismatch) in [(false, false), (true, false), (false, true)] {
        let fixture = GatewayFixture::new();
        *fixture.completion_failure.lock().unwrap() = failure;
        *fixture.completion_mismatch.lock().unwrap() = mismatch;
        let (gateway, task) = server(fixture.clone()).await;
        let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
        let commit = "a".repeat(40);
        let (first, second) = tokio::join!(
            gateway.complete_to_review(&claim, "run-1", &commit, b"{}"),
            gateway.complete_to_review(&claim, "run-1", &commit, b"{}")
        );
        assert_eq!(
            usize::from(first.is_ok()) + usize::from(second.is_ok()),
            usize::from(!failure && !mismatch)
        );
        assert_eq!(fixture.completions.lock().unwrap().len(), 1);
        assert!(
            gateway
                .complete_to_review(&claim, "run-1", &"a".repeat(40), b"{}")
                .await
                .is_err()
        );
        assert_eq!(fixture.completions.lock().unwrap().len(), 1);
        task.abort();
    }
}

#[test]
fn gateway_rejects_renderer_style_remote_or_credential_endpoints() {
    for endpoint in [
        "https://127.0.0.1:1234",
        "http://example.com:1234",
        "http://user:pass@127.0.0.1:1234",
        "http://127.0.0.1:1234/path",
        "http://127.0.0.1:1234/?secret=value",
    ] {
        assert!(AutomaticIssueGateway::new(endpoint.into()).is_err());
    }
}

#[tokio::test]
async fn heartbeat_advances_the_owned_revision_before_result_save_and_stops_after_completion() {
    let fixture = GatewayFixture::new();
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    gateway.heartbeat(&claim).await.unwrap();
    gateway.heartbeat(&claim).await.unwrap();
    assert_eq!(claim.revision(), 10);
    gateway
        .complete_to_review(&claim, "run-1", &"a".repeat(40), b"{}")
        .await
        .unwrap();
    assert_eq!(
        fixture.completions.lock().unwrap()[0].expected_revision,
        Some(10)
    );
    assert!(gateway.heartbeat(&claim).await.is_err());
    let requests = fixture.heartbeats.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].expected_revision, Some(8));
    assert_eq!(requests[1].expected_revision, Some(9));
    assert_ne!(requests[0].idempotency_key, requests[1].idempotency_key);
    task.abort();
}

#[tokio::test]
async fn unconfirmed_heartbeat_blocks_further_lease_and_completion_mutations() {
    let fixture = GatewayFixture::new();
    *fixture.heartbeat_failure.lock().unwrap() = true;
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    assert!(gateway.heartbeat(&claim).await.is_err());
    assert!(gateway.heartbeat(&claim).await.is_err());
    assert!(gateway.owned_revision(&claim).await.is_err());
    assert!(
        gateway
            .complete_to_review(&claim, "run-1", &"a".repeat(40), b"{}")
            .await
            .is_err()
    );
    assert_eq!(fixture.heartbeats.lock().unwrap().len(), 1);
    assert!(fixture.completions.lock().unwrap().is_empty());
    task.abort();
}

#[tokio::test]
async fn malformed_heartbeat_acks_never_authorize_a_completion() {
    for mutation in [
        None,
        Some(rpc::IssueGatewayMutation {
            issue_id: "other".into(),
            revision: 9,
        }),
        Some(rpc::IssueGatewayMutation {
            issue_id: "issue-1".into(),
            revision: 8,
        }),
    ] {
        let fixture = GatewayFixture::new();
        *fixture.heartbeat_response.lock().unwrap() = Some(rpc::IssueGatewayMutationResponse {
            outcome: mutation.map(rpc::issue_gateway_mutation_response::Outcome::Success),
        });
        let (gateway, task) = server(fixture.clone()).await;
        let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
        assert!(gateway.heartbeat(&claim).await.is_err());
        assert!(
            gateway
                .complete_to_review(&claim, "run-1", &"a".repeat(40), b"{}")
                .await
                .is_err()
        );
        assert!(fixture.completions.lock().unwrap().is_empty());
        task.abort();
    }
}

#[tokio::test]
async fn completion_waits_for_an_inflight_heartbeat_and_uses_its_confirmed_revision() {
    let fixture = GatewayFixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    *fixture.heartbeat_gate.lock().unwrap() = Some(gate.clone());
    let (gateway, task) = server(fixture.clone()).await;
    let gateway = Arc::new(gateway);
    let claim = Arc::new(gateway.claim("run-1", "issue-1", 7).await.unwrap());
    let heartbeat = tokio::spawn({
        let gateway = gateway.clone();
        let claim = claim.clone();
        async move { gateway.heartbeat(&claim).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fixture.heartbeats.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (polled, first_poll) = tokio::sync::oneshot::channel();
    let completion = tokio::spawn({
        let gateway = gateway.clone();
        let claim = claim.clone();
        async move {
            let commit = "a".repeat(40);
            let mut future = Box::pin(gateway.complete_to_review(&claim, "run-1", &commit, b"{}"));
            let mut polled = Some(polled);
            std::future::poll_fn(|context| {
                let result = std::future::Future::poll(future.as_mut(), context);
                if let Some(sender) = polled.take() {
                    sender.send(result.is_pending()).unwrap();
                }
                result
            })
            .await
        }
    });
    assert!(first_poll.await.unwrap());
    assert!(!completion.is_finished());
    assert!(fixture.completions.lock().unwrap().is_empty());
    gate.notify_one();
    heartbeat.await.unwrap().unwrap();
    completion.await.unwrap().unwrap();
    assert_eq!(
        fixture.completions.lock().unwrap()[0].expected_revision,
        Some(9)
    );
    task.abort();
}

#[tokio::test]
async fn interrupting_a_heartbeat_future_keeps_the_claim_unconfirmed() {
    let fixture = GatewayFixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    *fixture.heartbeat_gate.lock().unwrap() = Some(gate.clone());
    let (gateway, task) = server(fixture.clone()).await;
    let gateway = Arc::new(gateway);
    let claim = Arc::new(gateway.claim("run-1", "issue-1", 7).await.unwrap());
    let heartbeat = tokio::spawn({
        let gateway = gateway.clone();
        let claim = claim.clone();
        async move { gateway.heartbeat(&claim).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fixture.heartbeats.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    heartbeat.abort();
    assert!(heartbeat.await.unwrap_err().is_cancelled());
    assert!(gateway.heartbeat(&claim).await.is_err());
    assert!(
        gateway
            .complete_to_review(&claim, "run-1", &"a".repeat(40), b"{}")
            .await
            .is_err()
    );
    assert_eq!(fixture.heartbeats.lock().unwrap().len(), 1);
    assert!(fixture.completions.lock().unwrap().is_empty());
    gate.notify_one();
    task.abort();
}

#[tokio::test]
async fn result_admission_reserves_the_claim_without_waiting_for_a_network_response() {
    let fixture = GatewayFixture::new();
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    let pending = gateway
        .begin_complete_to_review(&claim, "run-1", &"a".repeat(40), b"{}")
        .await
        .unwrap();
    assert!(fixture.completions.lock().unwrap().is_empty());
    pending.finish().await.unwrap();
    assert_eq!(fixture.completions.lock().unwrap().len(), 1);
    assert!(
        gateway
            .complete_to_review(&claim, "run-1", &"a".repeat(40), b"{}")
            .await
            .is_err()
    );
    task.abort();
}

#[tokio::test]
async fn dropping_an_admitted_result_cannot_authorize_another_mutation() {
    let fixture = GatewayFixture::new();
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    let pending = gateway
        .begin_complete_to_review(&claim, "run-1", &"a".repeat(40), b"{}")
        .await
        .unwrap();
    drop(pending);
    assert!(gateway.heartbeat(&claim).await.is_err());
    assert!(
        gateway
            .complete_to_review(&claim, "run-1", &"a".repeat(40), b"{}")
            .await
            .is_err()
    );
    assert!(fixture.completions.lock().unwrap().is_empty());
    task.abort();
}

#[tokio::test]
async fn an_owned_revision_stays_stable_until_the_admission_guard_is_released() {
    let fixture = GatewayFixture::new();
    let (gateway, task) = server(fixture.clone()).await;
    let claim = gateway.claim("run-1", "issue-1", 7).await.unwrap();
    let owned = gateway.owned_revision(&claim).await.unwrap();
    assert_eq!(owned.run_id(), "run-1");
    assert_eq!(owned.issue_id(), "issue-1");
    assert_eq!(owned.revision(), 8);
    let mut heartbeat = Box::pin(gateway.heartbeat(&claim));
    assert!(
        std::future::poll_fn(|context| std::task::Poll::Ready(
            std::future::Future::poll(heartbeat.as_mut(), context).is_pending()
        ))
        .await
    );
    assert!(fixture.heartbeats.lock().unwrap().is_empty());
    drop(owned);
    heartbeat.await.unwrap();
    assert_eq!(gateway.owned_revision(&claim).await.unwrap().revision(), 9);
    gateway
        .complete_to_review(&claim, "run-1", &"a".repeat(40), b"{}")
        .await
        .unwrap();
    assert!(gateway.owned_revision(&claim).await.is_err());
    task.abort();
}
