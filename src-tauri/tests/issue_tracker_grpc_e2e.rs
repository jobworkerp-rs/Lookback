//! Opt-in acceptance tests for the current Issue Tracker v3 endpoint split.
//!
//! The tests deliberately use only the primary trusted-consumer endpoint, the
//! Gateway control endpoint, and the Gateway worker endpoint. They must never
//! revive the retired direct-manager surface.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use lookback_tauri_lib::{
    data::DataPaths,
    grpc::{
        connect,
        issue_tracker::{IssueTrackerGrpcService, ManagerWriteRequest, render_server_config},
        proto::issue_tracker::{manager::v1 as manager, rpc::v1 as rpc, v1 as issue},
    },
    jobworkerp::JobworkerpHandle,
};
use tonic::{Code, transport::Channel};
use tonic_reflection::pb::v1::{
    ServerReflectionRequest, server_reflection_client::ServerReflectionClient,
    server_reflection_request::MessageRequest,
};

mod common;
use common::{RunningIssueTrackerServer, require_env, reserve_loopback_port, tempdir};

const READY_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
#[ignore = "requires a staged issue-tracker-server; opt in with LOOKBACK_ISSUE_TRACKER_SERVER_BIN"]
async fn manager_consumer_create_accepts_a_stable_chat_uuid() {
    let server = start_server("issue-manager-create-chat").await;
    let service = IssueTrackerGrpcService::new(server.endpoint());
    service
        .health("chat-create-health")
        .await
        .expect("tracker health");
    // The chat mutation layer derives this UUID from the operation digest.
    let issue_id = "01234567-89ab-8def-8123-456789abcdef";
    let request = ManagerWriteRequest {
        correlation_id: "chat-create-uuid".into(),
        idempotency_key: "chat-create-uuid".into(),
        actor_audit: Some(manager::ActorAudit {
            actor_id: "lookback-user".into(),
            actor_kind: issue::ActorKind::Human as i32,
            reason: Some("explicit request".into()),
            ..Default::default()
        }),
        payload: manager::CreateIssueArgs {
            issue_id: issue_id.into(),
            issue: Some(issue::IssueDraft {
                title: "Issue from chat".into(),
                kind: issue::IssueKind::Task as i32,
                priority: issue::IssuePriority::P2 as i32,
                creation_basis: issue::CreationBasis::Explicit as i32,
                confirmation_status: issue::ConfirmationStatus::Confirmed as i32,
                ..Default::default()
            }),
            ..Default::default()
        },
    };
    let mutation = service.create_issue(request).await.expect("admit create");
    service
        .run_outbox_cycle("chat-create-outbox")
        .await
        .expect("apply create");
    let operation_id = mutation.operation.expect("operation").operation_id;
    let operation = service
        .get_operation("chat-create-operation", operation_id)
        .await
        .expect("durable operation");
    assert_eq!(
        operation.state,
        manager::DurableOperationState::Committed as i32
    );
    let created = service
        .get_issue_with_sync(
            "chat-create-read",
            manager::GetIssueWithSyncArgs {
                issue_id: issue_id.into(),
                ..Default::default()
            },
        )
        .await
        .expect("get created issue")
        .issue
        .expect("Issue");
    assert_eq!(created.title, "Issue from chat");
    assert_eq!(created.issue_id, issue_id);
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires a staged issue-tracker-server; opt in with LOOKBACK_ISSUE_TRACKER_SERVER_BIN"]
async fn gateway_awaiting_review_completion_uses_latest_claim_revision_and_confirms_canonical_state()
 {
    let fixture = start_auto_registered_issue_fixture("issue-gateway-awaiting-review").await;
    let AutoRegisteredIssueFixture {
        server,
        service,
        issue: issue_before,
        ..
    } = fixture;
    let channel = connect(&server.config.gateway_worker_url())
        .await
        .expect("connect Gateway worker endpoint");
    let mut gateway = rpc::issue_gateway_client::IssueGatewayClient::new(channel);
    let claim = gateway
        .claim(rpc::IssueGatewayClaimRequest {
            issue_id: issue_before.issue_id.clone(),
            expected_revision: Some(issue_before.revision),
            idempotency_key: "awaiting-review-claim".into(),
        })
        .await
        .expect("claim Issue")
        .into_inner();
    let claim = match claim.outcome {
        Some(rpc::issue_gateway_claim_response::Outcome::Success(value)) => value,
        other => panic!("expected Gateway claim, got {other:?}"),
    };
    let heartbeat = gateway
        .heartbeat(rpc::IssueGatewayHeartbeatRequest {
            issue_id: issue_before.issue_id.clone(),
            capability_id: claim.capability_id.clone(),
            expected_revision: Some(claim.revision),
            idempotency_key: "awaiting-review-latest-revision".into(),
        })
        .await
        .expect("refresh the worker revision")
        .into_inner();
    let latest_revision = match heartbeat.outcome {
        Some(rpc::issue_gateway_mutation_response::Outcome::Success(value)) => value.revision,
        other => panic!("expected Gateway heartbeat, got {other:?}"),
    };
    let completion = issue::AwaitingReviewCompletion {
        run_id: "run-awaiting-review-17".into(),
        full_commit_id: "b".repeat(40),
        evidence_references: vec!["artifact://run-awaiting-review-17/report.json".into()],
        report: Some(issue::CompletionReport {
            result: Some(issue::completion_report::Result::Verified(
                issue::VerifiedCompletion {
                    evidence: vec![issue::CompletionEvidence {
                        kind: issue::CompletionEvidenceKind::TestResult as i32,
                        summary: "required checks passed".into(),
                        uri: Some("artifact://run-awaiting-review-17/tests.json".into()),
                    }],
                },
            )),
        }),
    };

    let completed = service
        .complete_execution_to_awaiting_review(
            &server.config.gateway_worker_url(),
            &issue_before.issue_id,
            Some(&claim.capability_id),
            latest_revision,
            "awaiting-review-run-17-completion",
            completion.clone(),
        )
        .await
        .expect("dedicated completion RPC must be confirmed");
    assert_eq!(completed.revision, latest_revision + 1);
    assert_eq!(completed.run_id, completion.run_id);
    assert_eq!(completed.full_commit_id, completion.full_commit_id);
    assert_eq!(
        completed.evidence_references,
        completion.evidence_references
    );
    assert!(completed.lease_closed);

    // A later authoritative caller verifies the persisted replay marker with
    // the same request identity and no replacement process-local capability.
    let replay = service
        .complete_execution_to_awaiting_review(
            &server.config.gateway_worker_url(),
            &issue_before.issue_id,
            None,
            latest_revision,
            "awaiting-review-run-17-completion",
            completion,
        )
        .await
        .expect("exact idempotency request replays its persisted result");
    assert_eq!(replay, completed);

    let canonical = service
        .get_issue_with_sync(
            "awaiting-review-canonical-read",
            manager::GetIssueWithSyncArgs {
                issue_id: issue_before.issue_id,
                ..Default::default()
            },
        )
        .await
        .expect("read canonical Issue")
        .issue
        .expect("canonical Issue remains present");
    assert_eq!(canonical.state, issue::IssueState::AwaitingReview as i32);
    assert_eq!(canonical.revision, completed.revision);
    assert!(canonical.claim.is_none());
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires a staged issue-tracker-server; opt in with LOOKBACK_ISSUE_TRACKER_SERVER_BIN"]
async fn manager_consumer_update_issue_preserves_unmasked_fields_and_rejects_stale_revision() {
    let fixture = start_auto_registered_issue_fixture("issue-manager-update-chat").await;
    let AutoRegisteredIssueFixture {
        server,
        service,
        issue: original,
        ..
    } = fixture;
    let request = ManagerWriteRequest {
        correlation_id: "chat-update-test".into(),
        idempotency_key: "chat-update-test".into(),
        actor_audit: Some(manager::ActorAudit {
            actor_id: "lookback-chat-agent".into(),
            actor_kind: issue::ActorKind::InternalAgent as i32,
            reason: Some("human confirmed this issue through chat".into()),
            ..Default::default()
        }),
        payload: manager::UpdateIssueArgs {
            issue_id: original.issue_id.clone(),
            expected_revision: Some(original.revision),
            update_mask: vec![
                manager::ManagerIssueField::Title as i32,
                manager::ManagerIssueField::ConfirmationStatus as i32,
            ],
            patch: Some(manager::ManagerIssuePatch {
                title: Some("Updated by chat".into()),
                confirmation_status: Some(issue::ConfirmationStatus::Confirmed as i32),
                ..Default::default()
            }),
            ..Default::default()
        },
    };
    let mutation = service
        .update_issue(request.clone())
        .await
        .expect("admit update");
    service
        .run_outbox_cycle("chat-update-outbox")
        .await
        .expect("apply update");
    let operation_id = mutation.operation.expect("operation").operation_id;
    let awaited = service
        .await_operation(
            "chat-update-await",
            operation_id.clone(),
            Duration::from_secs(10),
        )
        .await;
    assert!(matches!(awaited, Ok(_) | Err(lookback_tauri_lib::grpc::issue_tracker::IssueTrackerClientError::UnconfirmedOperation(_))));
    let durable = service
        .get_operation("chat-update-operation", operation_id)
        .await
        .expect("read durable operation");
    assert_eq!(
        durable.state,
        manager::DurableOperationState::Committed as i32
    );
    let updated = service
        .get_issue_with_sync(
            "chat-update-read",
            manager::GetIssueWithSyncArgs {
                issue_id: original.issue_id.clone(),
                ..Default::default()
            },
        )
        .await
        .expect("get issue")
        .issue
        .expect("updated issue");
    assert_eq!(updated.title, "Updated by chat");
    assert_eq!(updated.description, original.description);
    assert_eq!(
        updated.confirmation_status,
        issue::ConfirmationStatus::Confirmed as i32
    );
    assert!(updated.revision > original.revision);

    let stale = service
        .update_issue(ManagerWriteRequest {
            idempotency_key: "chat-update-stale".into(),
            ..request
        })
        .await;
    if let Ok(mutation) = stale {
        service
            .run_outbox_cycle("chat-update-stale-outbox")
            .await
            .expect("drive stale update");
        assert!(
            service
                .await_operation(
                    "chat-update-stale-await",
                    mutation.operation.expect("stale operation").operation_id,
                    Duration::from_secs(10)
                )
                .await
                .is_err(),
            "stale operation must not commit"
        );
    }
    let after_stale = service
        .get_issue_with_sync(
            "chat-update-stale-read",
            manager::GetIssueWithSyncArgs {
                issue_id: original.issue_id,
                ..Default::default()
            },
        )
        .await
        .expect("get issue after stale write")
        .issue
        .expect("unchanged issue");
    assert_eq!(after_stale.revision, updated.revision);
    server.stop().await;
}

struct RunningAllInOne {
    child: tokio::process::Child,
    grpc_url: String,
}

fn staged_binary(directory: &Path, name: &str) -> PathBuf {
    std::fs::read_dir(directory)
        .expect("read staged sidecar directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.starts_with(&format!("{name}-")))
        })
        .unwrap_or_else(|| {
            panic!(
                "staged {name} binary is missing from {}",
                directory.display()
            )
        })
}

impl RunningAllInOne {
    async fn start(root: &Path, binary: &Path) -> Self {
        let data = DataPaths::with_root(root);
        data.ensure().expect("create jobworkerp data root");
        let port = reserve_loopback_port();
        let child = tokio::process::Command::new(binary)
            .current_dir(root)
            .env("STORAGE_TYPE", "Standalone")
            .env("STORAGE_RESTORE_AT_STARTUP", "false")
            .env("GRPC_ADDR", format!("127.0.0.1:{port}"))
            .env("SQLITE_URL", data.sqlite_url())
            .env("MCP_ENABLED", "false")
            .env("WORKER_DEFAULT_CONCURRENCY", "4")
            .env(
                "WORKER_CHANNELS",
                "llm,llm_external,llm_workflow,llm_batch,llm_periodic,embedding,embedding_workflow,rag,issue_tracker,issue_extract",
            )
            .env(
                "WORKER_CHANNEL_CONCURRENCIES",
                "1,2,1,2,1,1,1,2,1,1",
            )
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn all-in-one");
        let server = Self {
            child,
            grpc_url: format!("http://127.0.0.1:{port}"),
        };
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if JobworkerpHandle::connect(&server.grpc_url).await.is_ok() {
                return server;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "all-in-one did not become ready"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn stop(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

async fn start_server(label: &str) -> RunningIssueTrackerServer {
    let root = tempdir(label);
    let data = DataPaths::with_root(&root);
    RunningIssueTrackerServer::start(
        render_server_config(&data, reserve_loopback_port()).expect("render v3 server config"),
        require_env("LOOKBACK_ISSUE_TRACKER_SERVER_BIN"),
        READY_TIMEOUT,
        label,
    )
    .await
}

struct AutoRegisteredIssueFixture {
    server: RunningIssueTrackerServer,
    service: IssueTrackerGrpcService,
    candidate: manager::Candidate,
    issue: issue::Issue,
    mapping: manager::IssueMapping,
}

async fn start_auto_registered_issue_fixture(label: &str) -> AutoRegisteredIssueFixture {
    let server = start_server(label).await;
    let thread_id = format!("{label}-thread");
    let run_id = format!("{label}-run");
    let title = format!("Auto-registered issue for {label}");
    let channel = connect(&server.endpoint())
        .await
        .expect("connect primary endpoint");
    let mut extract = rpc::issue_extract_client::IssueExtractClient::new(channel);
    let applied = extract
        .apply_extraction(apply_request(
            &thread_id,
            &run_id,
            policy(true, true),
            vec![candidate(&title)],
        ))
        .await
        .expect("apply auto-register extraction")
        .into_inner();
    let (candidate_id, issue_id) = match applied.outcome {
        Some(rpc::apply_extraction_response::Outcome::Applied(value)) => (
            value
                .candidate_ids
                .into_iter()
                .next()
                .expect("created candidate"),
            value.issue_ids.into_iter().next().expect("created Issue"),
        ),
        other => panic!("expected applied auto-register extraction, got {other:?}"),
    };

    let service = IssueTrackerGrpcService::new(server.endpoint());
    service
        .health(&format!("{label}-health"))
        .await
        .expect("issue tracker health");
    let registration_id = format!("{label}-register");
    service
        .register_and_verify_gateway_policy(
            &server.config.gateway_control_url(),
            &registration_id,
            &registration_id,
            manager::WorkEligibilityMode::IncludeAutoRegistered,
        )
        .await
        .expect("register include-auto Gateway policy");

    let candidate = service
        .get_candidate(
            &format!("{label}-read-candidate"),
            manager::GetCandidateArgs {
                candidate_id,
                ..Default::default()
            },
        )
        .await
        .expect("read auto-registered candidate");
    let issue_with_sync = service
        .get_issue_with_sync(
            &format!("{label}-read-issue"),
            manager::GetIssueWithSyncArgs {
                issue_id,
                ..Default::default()
            },
        )
        .await
        .expect("read associated Issue and mapping");
    AutoRegisteredIssueFixture {
        server,
        service,
        candidate,
        issue: issue_with_sync.issue.expect("associated Issue"),
        mapping: issue_with_sync.mapping.expect("candidate-to-Issue mapping"),
    }
}

fn policy(extraction_enabled: bool, auto_register: bool) -> manager::ExtractionPolicy {
    manager::ExtractionPolicy {
        version: 1,
        extraction_enabled,
        auto_register_min_confidence: auto_register.then_some(0.90),
        auto_register_priority_range: auto_register.then_some(manager::ExtractionPriorityRange {
            lower: manager::CandidatePriority::P0 as i32,
            upper: manager::CandidatePriority::P2 as i32,
        }),
        explicit_only: true,
        allowed_kinds: vec![],
        extract_cap: 5,
    }
}

fn candidate(title: &str) -> rpc::ExtractCandidateInput {
    rpc::ExtractCandidateInput {
        title: title.into(),
        description: format!("{title} remains unfinished"),
        kind: "TASK".into(),
        suggested_priority: "P2".into(),
        suggested_state: "OPEN".into(),
        unfinished_reason: "The reported work is still open.".into(),
        completion: "The work is completed and verified.".into(),
        detection: "EXPLICIT".into(),
        confidence: 0.95,
        locator_memory_ids: vec!["memory-1".into()],
    }
}

fn apply_request(
    thread_id: &str,
    idempotency_key: &str,
    extraction_policy: manager::ExtractionPolicy,
    candidates: Vec<rpc::ExtractCandidateInput>,
) -> rpc::ApplyExtractionRequest {
    rpc::ApplyExtractionRequest {
        thread_id: thread_id.into(),
        summary_ref: format!("summary:{thread_id}"),
        snapshot_digest: "workflow".into(),
        extract_schema: "lookback-issue-extract.v1".into(),
        model_id: "issue-tracker-grpc-e2e".into(),
        prompt_version: "v3".into(),
        allowed_messages: vec![rpc::MessageDigest {
            memory_id: "memory-1".into(),
            content: "Please complete the reported task.".into(),
            ..Default::default()
        }],
        input_message_ids: vec!["memory-1".into()],
        candidates,
        mixed: false,
        failure_reason: None,
        idempotency_key: idempotency_key.into(),
        policy: Some(extraction_policy),
        rejected_candidate_ids_to_unsuppress: vec![],
        source_labels: vec!["team:issue-tracker-e2e".into()],
    }
}

async fn reflected_services(endpoint: &str) -> BTreeSet<String> {
    let channel = connect(endpoint)
        .await
        .expect("connect reflection endpoint");
    let mut client = ServerReflectionClient::new(channel);
    let request = ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::ListServices(String::new())),
    };
    let response = client
        .server_reflection_info(tokio_stream::iter([request]))
        .await
        .expect("start reflection stream")
        .into_inner()
        .message()
        .await
        .expect("read reflection response")
        .expect("reflection response");
    match response.message_response {
        Some(
            tonic_reflection::pb::v1::server_reflection_response::MessageResponse::ListServicesResponse(
                list,
            ),
        ) => list.service.into_iter().map(|service| service.name).collect(),
        other => panic!("unexpected reflection response: {other:?}"),
    }
}

async fn worker_rejects_trusted_consumer_services(endpoint: &str) {
    let channel: Channel = connect(endpoint)
        .await
        .expect("connect Gateway worker endpoint");
    let status =
        rpc::manager_consumer_service_client::ManagerConsumerServiceClient::new(channel.clone())
            .get_candidate(manager::GetCandidateArgs::default())
            .await
            .expect_err("worker endpoint must reject ManagerConsumerService");
    assert_eq!(status.code(), Code::Unimplemented);

    let status = rpc::manager_consumer_service_client::ManagerConsumerServiceClient::new(channel)
        .list_review_candidates(manager::ListReviewCandidatesArgs::default())
        .await
        .expect_err("worker endpoint must not expose ListReviewCandidates");
    assert_eq!(status.code(), Code::Unimplemented);

    let channel = connect(endpoint)
        .await
        .expect("reconnect Gateway worker endpoint");
    let status = rpc::issue_extract_client::IssueExtractClient::new(channel)
        .list_extraction_hints(rpc::ListExtractionHintsRequest::default())
        .await
        .expect_err("worker endpoint must reject IssueExtract");
    assert_eq!(status.code(), Code::Unimplemented);
}

#[tokio::test]
#[ignore = "requires a staged issue-tracker-server; opt in with LOOKBACK_ISSUE_TRACKER_SERVER_BIN"]
async fn phase5_6_primary_and_gateway_endpoints_enforce_the_v3_boundary() {
    let server = start_server("issue-tracker-v3-boundary").await;
    let primary = reflected_services(&server.config.primary_url()).await;

    assert!(primary.contains("issue_tracker.rpc.v1.ManagerConsumerService"));
    assert!(primary.contains("issue_tracker.rpc.v1.IssueExtract"));
    assert!(primary.contains("grpc.reflection.v1.ServerReflection"));
    worker_rejects_trusted_consumer_services(&server.config.gateway_worker_url()).await;

    let service = IssueTrackerGrpcService::new(server.endpoint());
    service
        .health("issue-tracker-v3-boundary-health")
        .await
        .expect("v3 health");
    service
        .register_and_verify_gateway_policy(
            &server.config.gateway_control_url(),
            "issue-tracker-v3-boundary-register",
            "issue-tracker-v3-boundary-register",
            manager::WorkEligibilityMode::ReviewedOnly,
        )
        .await
        .expect("register reviewed-only Gateway policy");

    let control_channel = connect(&server.config.gateway_control_url())
        .await
        .expect("connect Gateway control endpoint");
    let status =
        rpc::manager_consumer_service_client::ManagerConsumerServiceClient::new(control_channel)
            .list_review_candidates(manager::ListReviewCandidatesArgs::default())
            .await
            .expect_err("control endpoint must not expose ListReviewCandidates");
    assert_eq!(status.code(), Code::Unimplemented);

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires a staged issue-tracker-server; opt in with LOOKBACK_ISSUE_TRACKER_SERVER_BIN"]
async fn phase5_6_apply_extraction_and_gateway_claim_lifecycle_are_idempotent() {
    let server = start_server("issue-tracker-v3-acceptance").await;
    let channel = connect(&server.endpoint())
        .await
        .expect("connect primary endpoint");
    let mut extract = rpc::issue_extract_client::IssueExtractClient::new(channel);

    let skipped = extract
        .apply_extraction(apply_request(
            "disabled-thread",
            "disabled-run",
            policy(false, false),
            vec![],
        ))
        .await
        .expect("apply disabled extraction")
        .into_inner();
    assert!(matches!(
        skipped.outcome,
        Some(rpc::apply_extraction_response::Outcome::Skipped(_))
    ));

    let candidate_only = extract
        .apply_extraction(apply_request(
            "review-thread",
            "review-run",
            policy(true, false),
            vec![candidate("Review-only candidate")],
        ))
        .await
        .expect("apply review-only extraction")
        .into_inner();
    let candidate_only = match candidate_only.outcome {
        Some(rpc::apply_extraction_response::Outcome::Applied(value)) => value,
        other => panic!("expected applied review-only extraction, got {other:?}"),
    };
    assert_eq!(candidate_only.candidate_ids.len(), 1);
    assert!(candidate_only.issue_ids.is_empty());

    let auto_request = apply_request(
        "auto-thread",
        "auto-run",
        policy(true, true),
        vec![candidate("Auto-registered candidate")],
    );
    let applied = extract
        .apply_extraction(auto_request.clone())
        .await
        .expect("apply auto-register extraction")
        .into_inner();
    let applied = match applied.outcome {
        Some(rpc::apply_extraction_response::Outcome::Applied(value)) => value,
        other => panic!("expected applied auto-register extraction, got {other:?}"),
    };
    assert_eq!(applied.candidate_ids.len(), 1, "{applied:?}");
    assert_eq!(applied.issue_ids.len(), 1, "{applied:?}");

    let replay = extract
        .apply_extraction(auto_request)
        .await
        .expect("replay auto-register extraction")
        .into_inner();
    let replay = match replay.outcome {
        Some(rpc::apply_extraction_response::Outcome::Applied(value)) => value,
        other => panic!("expected applied replay, got {other:?}"),
    };
    assert_eq!(replay.run_id, applied.run_id);
    assert_eq!(replay.issue_ids, applied.issue_ids);

    let hints = extract
        .list_extraction_hints(rpc::ListExtractionHintsRequest {
            thread_id: "review-thread".into(),
        })
        .await
        .expect("list extraction hints")
        .into_inner();
    assert!(matches!(
        hints.outcome,
        Some(rpc::list_extraction_hints_response::Outcome::Success(_))
    ));

    let service = IssueTrackerGrpcService::new(server.endpoint());
    service
        .health("issue-tracker-v3-acceptance-health")
        .await
        .expect("v3 health");
    let review_page = service
        .list_review_candidates(
            "issue-tracker-v3-review-list",
            manager::ListReviewCandidatesArgs {
                page: Some(manager::PageRequest {
                    page_size: 10,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .expect("list review candidates through primary endpoint");
    assert!(review_page.rows.iter().any(|row| {
        row.candidate
            .as_ref()
            .is_some_and(|candidate| candidate.candidate_id == candidate_only.candidate_ids[0])
    }));
    let auto_row = review_page
        .rows
        .iter()
        .find(|row| {
            row.candidate
                .as_ref()
                .is_some_and(|candidate| candidate.candidate_id == applied.candidate_ids[0])
        })
        .expect("auto-registered candidate remains reviewable");
    assert_eq!(
        auto_row.integrity,
        manager::ReviewCandidateIntegrity::Normal as i32
    );
    assert_eq!(
        auto_row.resolution_state,
        manager::ReviewCandidateResolutionState::Actionable as i32
    );
    assert_eq!(
        auto_row
            .associated_issue
            .as_ref()
            .map(|issue| issue.issue_id.as_str()),
        Some(applied.issue_ids[0].as_str())
    );
    service
        .register_and_verify_gateway_policy(
            &server.config.gateway_control_url(),
            "issue-tracker-v3-include-auto",
            "issue-tracker-v3-include-auto",
            manager::WorkEligibilityMode::IncludeAutoRegistered,
        )
        .await
        .expect("register include-auto Gateway policy");

    let channel = connect(&server.config.gateway_worker_url())
        .await
        .expect("connect Gateway worker endpoint");
    let mut gateway = rpc::issue_gateway_client::IssueGatewayClient::new(channel);
    let ready = gateway
        .list_ready(rpc::IssueGatewayListReadyRequest {
            page_size: 10,
            page_token: None,
        })
        .await
        .expect("list Gateway-ready issues")
        .into_inner();
    let ready = match ready.outcome {
        Some(rpc::issue_gateway_list_ready_response::Outcome::Success(value)) => value,
        other => panic!("expected Gateway ready page, got {other:?}"),
    };
    let ready_issue = ready
        .issues
        .into_iter()
        .find(|item| {
            item.issue
                .as_ref()
                .is_some_and(|issue| issue.issue_id == applied.issue_ids[0])
        })
        .expect("auto-registered issue is Gateway-ready");
    let issue = ready_issue.issue.expect("ready issue");
    let claimed = gateway
        .claim(rpc::IssueGatewayClaimRequest {
            issue_id: issue.issue_id.clone(),
            expected_revision: Some(issue.revision),
            idempotency_key: "claim-auto-issue".into(),
        })
        .await
        .expect("claim auto-registered issue")
        .into_inner();
    let claimed = match claimed.outcome {
        Some(rpc::issue_gateway_claim_response::Outcome::Success(value)) => value,
        other => panic!("expected Gateway claim, got {other:?}"),
    };
    let heartbeat = rpc::IssueGatewayHeartbeatRequest {
        issue_id: claimed.issue_id.clone(),
        capability_id: claimed.capability_id.clone(),
        expected_revision: Some(claimed.revision),
        idempotency_key: "heartbeat-auto-issue".into(),
    };
    let first_heartbeat = gateway
        .heartbeat(heartbeat.clone())
        .await
        .expect("heartbeat claim")
        .into_inner();
    let replay_heartbeat = gateway
        .heartbeat(heartbeat)
        .await
        .expect("replay heartbeat")
        .into_inner();
    assert_eq!(first_heartbeat.outcome, replay_heartbeat.outcome);

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires a staged issue-tracker-server; opt in with LOOKBACK_ISSUE_TRACKER_SERVER_BIN"]
async fn rejected_candidate_is_durably_confirmed_by_the_client() {
    let server = start_server("issue-tracker-rejected-candidate-confirmation").await;
    let channel = connect(&server.endpoint())
        .await
        .expect("connect primary endpoint");
    let mut extract = rpc::issue_extract_client::IssueExtractClient::new(channel);
    let applied = extract
        .apply_extraction(apply_request(
            "rejected-candidate-thread",
            "rejected-candidate-run",
            policy(true, false),
            vec![candidate("Candidate rejected during review")],
        ))
        .await
        .expect("apply extraction")
        .into_inner();
    let candidate_id = match applied.outcome {
        Some(rpc::apply_extraction_response::Outcome::Applied(value)) => value
            .candidate_ids
            .into_iter()
            .next()
            .expect("created candidate"),
        other => panic!("expected applied extraction, got {other:?}"),
    };

    let service = IssueTrackerGrpcService::new(server.endpoint());
    service
        .health("rejected-candidate-confirmation-health")
        .await
        .expect("issue tracker health");
    let candidate = service
        .get_candidate(
            "rejected-candidate-read",
            manager::GetCandidateArgs {
                candidate_id: candidate_id.clone(),
                ..Default::default()
            },
        )
        .await
        .expect("read proposed candidate");
    let receipt = service
        .resolve_candidate(ManagerWriteRequest {
            correlation_id: "rejected-candidate-resolve".into(),
            idempotency_key: "rejected-candidate-resolve".into(),
            actor_audit: Some(manager::ActorAudit {
                actor_id: "issue-tracker-e2e".into(),
                actor_kind: lookback_tauri_lib::grpc::proto::issue_tracker::v1::ActorKind::Human
                    as i32,
                reason: Some("not actionable".into()),
                ..Default::default()
            }),
            payload: manager::ResolveCandidateArgs {
                candidate_id: candidate_id.clone(),
                expected_revision: Some(candidate.revision),
                decision: Some(manager::resolve_candidate_args::Decision::Reject(
                    manager::RejectResolution {
                        reason_category: manager::ResolutionReasonCategory::NotActionable as i32,
                        rationale: Some("not actionable".into()),
                    },
                )),
                ..Default::default()
            },
        })
        .await
        .expect("resolve rejected candidate");
    let operation = service
        .await_operation(
            "rejected-candidate-await",
            receipt.operation_id,
            Duration::from_secs(5),
        )
        .await
        .expect("rejected candidate has a confirmed result snapshot");
    assert!(matches!(
        operation.terminal_snapshot,
        Some(manager::durable_operation::TerminalSnapshot::ResultSnapshot(_))
    ));

    let rejected = service
        .get_candidate(
            "rejected-candidate-read-after-resolution",
            manager::GetCandidateArgs {
                candidate_id,
                ..Default::default()
            },
        )
        .await
        .expect("read rejected candidate");
    assert_eq!(
        rejected.status,
        manager::CandidateStatus::Rejected as i32,
        "the confirmed operation must include the persisted rejection"
    );

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires a staged issue-tracker-server; opt in with LOOKBACK_ISSUE_TRACKER_SERVER_BIN"]
async fn resolve_candidate_replay_is_idempotent_and_key_reuse_with_a_new_request_is_rejected() {
    let server = start_server("issue-tracker-resolution-idempotency").await;
    let channel = connect(&server.endpoint())
        .await
        .expect("connect primary endpoint");
    let mut extract = rpc::issue_extract_client::IssueExtractClient::new(channel);
    let applied = extract
        .apply_extraction(apply_request(
            "resolution-idempotency-thread",
            "resolution-idempotency-run",
            policy(true, false),
            vec![candidate("Candidate resolved once")],
        ))
        .await
        .expect("apply extraction")
        .into_inner();
    let candidate_id = match applied.outcome {
        Some(rpc::apply_extraction_response::Outcome::Applied(value)) => value
            .candidate_ids
            .into_iter()
            .next()
            .expect("created candidate"),
        other => panic!("expected applied extraction, got {other:?}"),
    };

    let service = IssueTrackerGrpcService::new(server.endpoint());
    service
        .health("resolution-idempotency-health")
        .await
        .expect("issue tracker health");
    let candidate = service
        .get_candidate(
            "resolution-idempotency-read",
            manager::GetCandidateArgs {
                candidate_id: candidate_id.clone(),
                ..Default::default()
            },
        )
        .await
        .expect("read proposed candidate");
    let request = ManagerWriteRequest {
        correlation_id: "resolution-idempotency-resolve".into(),
        idempotency_key: "resolution-idempotency-key".into(),
        actor_audit: Some(manager::ActorAudit {
            actor_id: "issue-tracker-e2e".into(),
            actor_kind: lookback_tauri_lib::grpc::proto::issue_tracker::v1::ActorKind::Human as i32,
            reason: Some("reject candidate once".into()),
            ..Default::default()
        }),
        payload: manager::ResolveCandidateArgs {
            candidate_id: candidate_id.clone(),
            expected_revision: Some(candidate.revision),
            decision: Some(manager::resolve_candidate_args::Decision::Reject(
                manager::RejectResolution {
                    reason_category: manager::ResolutionReasonCategory::NotActionable as i32,
                    rationale: Some("not actionable".into()),
                },
            )),
            ..Default::default()
        },
    };
    let first = service
        .resolve_candidate(request.clone())
        .await
        .expect("resolve candidate");
    service
        .await_operation(
            "resolution-idempotency-await-first",
            first.operation_id.clone(),
            Duration::from_secs(5),
        )
        .await
        .expect("first resolution is durably committed");
    let after_first = service
        .get_candidate(
            "resolution-idempotency-read-after-first",
            manager::GetCandidateArgs {
                candidate_id: candidate_id.clone(),
                ..Default::default()
            },
        )
        .await
        .expect("read rejected candidate");

    let replay = service
        .resolve_candidate(request.clone())
        .await
        .expect("replay identical candidate resolution");
    assert_eq!(replay.operation_id, first.operation_id);
    let after_replay = service
        .get_candidate(
            "resolution-idempotency-read-after-replay",
            manager::GetCandidateArgs {
                candidate_id: candidate_id.clone(),
                ..Default::default()
            },
        )
        .await
        .expect("read candidate after replay");
    assert_eq!(
        after_replay.status,
        manager::CandidateStatus::Rejected as i32
    );
    assert_eq!(after_replay.revision, after_first.revision);

    let mut different_request = request;
    different_request.payload.decision = Some(manager::resolve_candidate_args::Decision::Reject(
        manager::RejectResolution {
            reason_category: manager::ResolutionReasonCategory::Duplicate as i32,
            rationale: Some("this is a different decision".into()),
        },
    ));
    let error = service
        .resolve_candidate(different_request)
        .await
        .expect_err("same key with a different request must be rejected");
    match error {
        lookback_tauri_lib::grpc::issue_tracker::IssueTrackerClientError::Manager(error) => {
            assert_eq!(
                error.code,
                manager::ManagerErrorCode::IdempotencyConflict as i32
            );
        }
        other => panic!("expected idempotency conflict, got {other}"),
    }

    let after_conflict = service
        .get_candidate(
            "resolution-idempotency-read-after-conflict",
            manager::GetCandidateArgs {
                candidate_id: candidate_id.clone(),
                ..Default::default()
            },
        )
        .await
        .expect("read candidate after key conflict");
    assert_eq!(
        after_conflict.status,
        manager::CandidateStatus::Rejected as i32
    );
    assert_eq!(after_conflict.revision, after_first.revision);

    let history = service
        .list_operation_history(
            "resolution-idempotency-history",
            manager::ListOperationHistoryArgs {
                page: Some(manager::PageRequest {
                    page_size: 10,
                    ..Default::default()
                }),
                filter: Some(manager::OperationHistoryFilter {
                    direct_target: Some(
                        manager::operation_history_filter::DirectTarget::CandidateId(candidate_id),
                    ),
                    method: Some("manager_resolve_candidate".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .expect("list candidate-resolution operations");
    assert_eq!(history.entries.len(), 1, "{history:?}");
    assert_eq!(history.entries[0].operation_id, first.operation_id);

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires a staged issue-tracker-server; opt in with LOOKBACK_ISSUE_TRACKER_SERVER_BIN"]
async fn auto_registered_candidate_can_be_confirmed_done_with_unverified_completion() {
    let fixture = start_auto_registered_issue_fixture("issue-tracker-auto-done-unverified").await;
    let AutoRegisteredIssueFixture {
        server,
        service,
        candidate: candidate_before,
        issue: issue_before,
        mapping: mapping_before,
    } = fixture;
    let candidate_id = candidate_before.candidate_id.clone();
    let issue_id = issue_before.issue_id.clone();
    assert_eq!(
        candidate_before.status,
        manager::CandidateStatus::AutoRegistered as i32
    );
    assert_eq!(
        mapping_before.candidate_id.as_deref(),
        Some(candidate_id.as_str())
    );
    assert_eq!(mapping_before.issue_id, issue_id);
    assert_eq!(issue_before.state, issue::IssueState::Open as i32);

    let receipt = service
        .resolve_candidate(ManagerWriteRequest {
            correlation_id: "auto-done-unverified-resolve".into(),
            idempotency_key: "auto-done-unverified-resolution".into(),
            actor_audit: Some(manager::ActorAudit {
                actor_id: "issue-tracker-e2e".into(),
                actor_kind: issue::ActorKind::InternalAgent as i32,
                reason: Some("automatic completion with residual risk".into()),
                ..Default::default()
            }),
            payload: manager::ResolveCandidateArgs {
                candidate_id: candidate_id.clone(),
                expected_revision: Some(candidate_before.revision),
                decision: Some(
                    manager::resolve_candidate_args::Decision::ConfirmAssociatedIssue(
                        manager::ConfirmAssociatedIssueResolution {
                            issue_id: issue_id.clone(),
                            expected_revision: Some(issue_before.revision),
                            update_mask: vec![
                                manager::ManagerIssueField::ConfirmationStatus as i32,
                            ],
                            patch: Some(manager::ManagerIssuePatch {
                                confirmation_status:
                                    Some(issue::ConfirmationStatus::Confirmed as i32),
                                ..Default::default()
                            }),
                            basis: manager::ResolutionBasis::AutoReview as i32,
                            target_state: Some(issue::IssueState::Done as i32),
                            terminal_report: Some(
                                manager::confirm_associated_issue_resolution::TerminalReport::
                                    Completion(issue::CompletionReport {
                                        result: Some(
                                            issue::completion_report::Result::Unverified(
                                                issue::UnverifiedCompletion {
                                                    reason: "The follow-up reports completion."
                                                        .into(),
                                                    residual_risk:
                                                        "Independent verification remains.".into(),
                                                },
                                            ),
                                        ),
                                    }),
                            ),
                            ..Default::default()
                        },
                    ),
                ),
                ..Default::default()
            },
        })
        .await
        .expect("admit AUTO_REGISTERED completion resolution");
    service
        .run_outbox_cycle("auto-done-unverified-outbox")
        .await
        .expect("drive completion resolution outbox");
    let operation = service
        .await_operation(
            "auto-done-unverified-await",
            receipt.operation_id,
            Duration::from_secs(10),
        )
        .await
        .expect("auto-DONE resolution commits");
    assert_eq!(
        operation.state,
        manager::DurableOperationState::Committed as i32
    );

    let candidate_after = service
        .get_candidate(
            "auto-done-unverified-read-candidate-after",
            manager::GetCandidateArgs {
                candidate_id: candidate_id.clone(),
                ..Default::default()
            },
        )
        .await
        .expect("read resolved candidate");
    let issue_after = service
        .get_issue_with_sync(
            "auto-done-unverified-read-issue-after",
            manager::GetIssueWithSyncArgs {
                issue_id,
                ..Default::default()
            },
        )
        .await
        .expect("read completed Issue and mapping");
    let issue_after_value = issue_after.issue.expect("completed Issue remains present");
    let mapping_after = issue_after
        .mapping
        .expect("candidate-to-Issue mapping remains");
    assert_eq!(
        candidate_after.status,
        manager::CandidateStatus::Accepted as i32
    );
    assert!(candidate_after.revision > candidate_before.revision);
    assert_eq!(issue_after_value.state, issue::IssueState::Done as i32);
    assert!(issue_after_value.revision > issue_before.revision);
    assert_eq!(
        issue_after_value.confirmation_status,
        issue::ConfirmationStatus::Confirmed as i32
    );
    assert!(issue_after_value.completed_at.is_some());
    assert_eq!(
        mapping_after.candidate_id.as_deref(),
        Some(candidate_id.as_str())
    );
    assert_eq!(mapping_after.issue_id, issue_after_value.issue_id);

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires a staged issue-tracker-server; opt in with LOOKBACK_ISSUE_TRACKER_SERVER_BIN"]
async fn auto_registered_false_positive_rejects_candidate_and_cancels_issue() {
    let fixture = start_auto_registered_issue_fixture("issue-tracker-auto-false-positive").await;
    let AutoRegisteredIssueFixture {
        server,
        service,
        candidate: candidate_before,
        issue: issue_before,
        mapping: mapping_before,
    } = fixture;
    let candidate_id = candidate_before.candidate_id.clone();
    let issue_id = issue_before.issue_id.clone();
    assert_eq!(
        candidate_before.status,
        manager::CandidateStatus::AutoRegistered as i32
    );
    assert_eq!(
        mapping_before.candidate_id.as_deref(),
        Some(candidate_id.as_str())
    );
    assert_eq!(mapping_before.issue_id, issue_id);
    assert_eq!(issue_before.state, issue::IssueState::Open as i32);

    let receipt = service
        .resolve_candidate(ManagerWriteRequest {
            correlation_id: "auto-false-positive-resolve".into(),
            idempotency_key: "auto-false-positive-resolution".into(),
            actor_audit: Some(manager::ActorAudit {
                actor_id: "issue-tracker-e2e".into(),
                actor_kind: issue::ActorKind::InternalAgent as i32,
                reason: Some("automatic false-positive cancellation".into()),
                ..Default::default()
            }),
            payload: manager::ResolveCandidateArgs {
                candidate_id: candidate_id.clone(),
                expected_revision: Some(candidate_before.revision),
                decision: Some(
                    manager::resolve_candidate_args::Decision::CancelAssociatedIssue(
                        manager::CancelAssociatedIssueResolution {
                            issue_id: issue_id.clone(),
                            expected_issue_revision: Some(issue_before.revision),
                            expected_mapping_candidate_id: candidate_id.clone(),
                            expected_mapping_issue_id: issue_id.clone(),
                            cancellation_report: Some(issue::CancellationReport {
                                classification: issue::CancellationClass::FalsePositive as i32,
                            }),
                            reason_category: manager::ResolutionReasonCategory::NotActionable
                                as i32,
                            rationale: Some("The detected work is not actionable.".into()),
                        },
                    ),
                ),
                ..Default::default()
            },
        })
        .await
        .expect("admit AUTO_REGISTERED false-positive resolution");
    service
        .run_outbox_cycle("auto-false-positive-outbox")
        .await
        .expect("drive false-positive resolution outbox");
    let operation = service
        .await_operation(
            "auto-false-positive-await",
            receipt.operation_id,
            Duration::from_secs(10),
        )
        .await
        .expect("false-positive resolution commits");
    assert_eq!(
        operation.state,
        manager::DurableOperationState::Committed as i32
    );

    let candidate_after = service
        .get_candidate(
            "auto-false-positive-read-candidate-after",
            manager::GetCandidateArgs {
                candidate_id: candidate_id.clone(),
                ..Default::default()
            },
        )
        .await
        .expect("read rejected candidate");
    let issue_after = service
        .get_issue_with_sync(
            "auto-false-positive-read-issue-after",
            manager::GetIssueWithSyncArgs {
                issue_id,
                ..Default::default()
            },
        )
        .await
        .expect("read canceled Issue and mapping");
    let issue_after_value = issue_after.issue.expect("canceled Issue remains present");
    assert!(
        issue_after.mapping.is_none(),
        "a canceled false-positive Issue must not retain an active candidate mapping"
    );
    assert_eq!(
        candidate_after.status,
        manager::CandidateStatus::Rejected as i32
    );
    assert_eq!(issue_after_value.state, issue::IssueState::Canceled as i32);
    assert!(candidate_after.revision > candidate_before.revision);
    assert!(issue_after_value.revision > issue_before.revision);

    let history = service
        .list_operation_history(
            "auto-false-positive-history",
            manager::ListOperationHistoryArgs {
                page: Some(manager::PageRequest {
                    page_size: 10,
                    ..Default::default()
                }),
                filter: Some(manager::OperationHistoryFilter {
                    direct_target: Some(
                        manager::operation_history_filter::DirectTarget::CandidateId(candidate_id),
                    ),
                    method: Some("manager_resolve_candidate".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .expect("read false-positive resolution audit");
    assert_eq!(history.entries.len(), 1, "{history:?}");
    assert_eq!(
        history.entries[0].resolution_decision_kind,
        Some(manager::ResolutionDecisionKind::CancelAssociatedIssue as i32)
    );
    assert_eq!(
        history.entries[0].resolution_reason_category,
        Some(manager::ResolutionReasonCategory::NotActionable as i32)
    );

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires a staged issue-tracker-server; opt in with LOOKBACK_ISSUE_TRACKER_SERVER_BIN"]
async fn claim_after_issue_read_prevents_auto_done_resolution_from_committing() {
    let fixture = start_auto_registered_issue_fixture("issue-tracker-auto-done-claim-race").await;
    let AutoRegisteredIssueFixture {
        server,
        service,
        candidate: candidate_before,
        issue: issue_before,
        mapping: mapping_before,
    } = fixture;
    let candidate_id = candidate_before.candidate_id.clone();
    let issue_id = issue_before.issue_id.clone();
    assert_eq!(
        candidate_before.status,
        manager::CandidateStatus::AutoRegistered as i32
    );
    assert_eq!(
        mapping_before.candidate_id.as_deref(),
        Some(candidate_id.as_str())
    );
    assert_eq!(mapping_before.issue_id, issue_id);
    assert!(issue_before.claim.is_none());

    let channel = connect(&server.config.gateway_worker_url())
        .await
        .expect("connect Gateway worker endpoint");
    let mut gateway = rpc::issue_gateway_client::IssueGatewayClient::new(channel);
    let claimed = gateway
        .claim(rpc::IssueGatewayClaimRequest {
            issue_id: issue_id.clone(),
            expected_revision: Some(issue_before.revision),
            idempotency_key: "auto-done-claim-race-claim".into(),
        })
        .await
        .expect("claim Issue after reading it")
        .into_inner();
    let claimed = match claimed.outcome {
        Some(rpc::issue_gateway_claim_response::Outcome::Success(value)) => value,
        other => panic!("expected successful Gateway claim, got {other:?}"),
    };
    assert_eq!(claimed.issue_id, issue_id);
    assert!(claimed.revision > issue_before.revision);
    assert!(!claimed.capability_id.is_empty());

    let receipt = service
        .resolve_candidate(ManagerWriteRequest {
            correlation_id: "auto-done-claim-race-resolve".into(),
            idempotency_key: "auto-done-claim-race-resolution".into(),
            actor_audit: Some(manager::ActorAudit {
                actor_id: "issue-tracker-e2e".into(),
                actor_kind: issue::ActorKind::InternalAgent as i32,
                reason: Some("automatic completion from later source evidence".into()),
                ..Default::default()
            }),
            payload: manager::ResolveCandidateArgs {
                candidate_id: candidate_id.clone(),
                expected_revision: Some(candidate_before.revision),
                decision: Some(
                    manager::resolve_candidate_args::Decision::ConfirmAssociatedIssue(
                        manager::ConfirmAssociatedIssueResolution {
                            issue_id: issue_id.clone(),
                            expected_revision: Some(issue_before.revision),
                            update_mask: vec![
                                manager::ManagerIssueField::ConfirmationStatus as i32,
                            ],
                            patch: Some(manager::ManagerIssuePatch {
                                confirmation_status:
                                    Some(issue::ConfirmationStatus::Confirmed as i32),
                                ..Default::default()
                            }),
                            basis: manager::ResolutionBasis::AutoReview as i32,
                            target_state: Some(issue::IssueState::Done as i32),
                            terminal_report: Some(
                                manager::confirm_associated_issue_resolution::TerminalReport::
                                    Completion(issue::CompletionReport {
                                        result: Some(
                                            issue::completion_report::Result::Verified(
                                                issue::VerifiedCompletion {
                                                    evidence: vec![issue::CompletionEvidence {
                                                        kind: issue::CompletionEvidenceKind::
                                                            OtherEvidence
                                                            as i32,
                                                        summary:
                                                            "Follow-up evidence verifies completion."
                                                                .into(),
                                                        ..Default::default()
                                                    }],
                                                },
                                            ),
                                        ),
                                    }),
                            ),
                            ..Default::default()
                        },
                    ),
                ),
                ..Default::default()
            },
        })
        .await
        .expect("admit candidate resolution against the previously read Issue");
    let operation_result = service
        .await_operation(
            "auto-done-claim-race-await",
            receipt.operation_id.clone(),
            Duration::from_secs(5),
        )
        .await;
    let operation = match operation_result {
        Ok(operation) => operation,
        Err(
            lookback_tauri_lib::grpc::issue_tracker::IssueTrackerClientError::UnconfirmedOperation(
                _,
            ),
        ) => service
            .get_operation(
                "auto-done-claim-race-read-operation",
                receipt.operation_id.clone(),
            )
            .await
            .expect("read rejected resolution operation"),
        Err(error) => panic!("wait for candidate resolution: {error}"),
    };
    assert_ne!(
        operation.state,
        manager::DurableOperationState::Committed as i32,
        "a claimed Issue must not be committed as automatically DONE"
    );

    let issue_after = service
        .get_issue_with_sync(
            "auto-done-claim-race-read-after-resolution",
            manager::GetIssueWithSyncArgs {
                issue_id,
                ..Default::default()
            },
        )
        .await
        .expect("read Issue after rejected auto-DONE")
        .issue
        .expect("associated Issue remains present");
    assert_eq!(issue_after.state, issue::IssueState::InProgress as i32);
    assert!(
        issue_after.claim.is_some(),
        "the acquired claim remains active"
    );
    assert_eq!(issue_after.revision, claimed.revision);
    let candidate_after = service
        .get_candidate(
            "auto-done-claim-race-read-candidate-after-resolution",
            manager::GetCandidateArgs {
                candidate_id,
                ..Default::default()
            },
        )
        .await
        .expect("read candidate after rejected auto-DONE");
    assert_eq!(
        candidate_after.status,
        manager::CandidateStatus::AutoRegistered as i32
    );
    assert_eq!(candidate_after.revision, candidate_before.revision);

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires staged issue-tracker-server and all-in-one sidecars"]
async fn phase5_6_dynamic_grpc_runner_records_issue_extract_failures_through_reflection() {
    let server = start_server("issue-tracker-v3-workflow").await;
    let worker_root = tempdir("issue-tracker-v3-workflow-runner");
    let bin_dir = require_env("LOOKBACK_SIDECAR_BIN_DIR");
    let all_in_one =
        RunningAllInOne::start(&worker_root, &staged_binary(&bin_dir, "all-in-one")).await;
    let handle = JobworkerpHandle::connect(&all_in_one.grpc_url)
        .await
        .expect("connect all-in-one");
    let registered = handle
        .register_workers_from_yaml(
            &lookback_tauri_lib::data::paths::lookback_issue_extract_workers_yaml()
                .expect("resolve issue-extract worker YAML"),
        )
        .await
        .expect("register issue-extract workflow worker");
    assert!(registered.contains_key("lookback-issue-extract"));

    let workflow_input = serde_json::json!({
        "thread_id": "workflow-failure-thread",
        "idempotency_key": "workflow-failure-run",
        "issue_tracker_grpc_host": "127.0.0.1",
        "issue_tracker_grpc_port": server.port(),
        "memories_grpc_host": "127.0.0.1",
        "memories_grpc_port": 1,
        "policy_json": serde_json::json!({
            "version": 1,
            "extraction_enabled": true,
            "auto_register_min_confidence": null,
            "explicit_only": true,
            "allowed_kinds": [],
            "extract_cap": 5
        }).to_string(),
        "summary_json": { "has_unfinished_issue_candidates": true }
    });
    let _ = handle
        .dispatch_unary(
            "lookback-issue-extract",
            serde_json::json!({ "input": workflow_input.to_string() }),
            Some("run"),
        )
        .await;

    let service = IssueTrackerGrpcService::new(server.endpoint());
    service
        .health("issue-tracker-v3-workflow-health")
        .await
        .expect("v3 health");
    let runs = service
        .list_extraction_runs(
            "issue-tracker-v3-workflow-runs",
            manager::ListExtractionRunsArgs {
                page: Some(manager::PageRequest {
                    page_size: 20,
                    page_token: None,
                }),
                filter: Some(manager::ExtractionRunFilter::default()),
                ..Default::default()
            },
        )
        .await
        .expect("list extraction runs created by the workflow");
    assert!(
        runs.runs.iter().any(|run| {
            run.state == manager::ExtractionRunState::Failed as i32
                && run.error.as_deref().is_some_and(|error| !error.is_empty())
        }),
        "workflow catch path must record a failed ApplyExtraction run: {runs:?}"
    );

    all_in_one.stop().await;
    server.stop().await;
}
