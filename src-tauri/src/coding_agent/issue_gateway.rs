use crate::grpc::{
    issue_tracker::IssueTrackerGrpcService,
    proto::issue_tracker::{rpc::v1 as rpc, v1 as issue},
};
use prost::Message;
use sha2::{Digest, Sha256};
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use thiserror::Error;

pub const MAX_ISSUE_RESULT_ACK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomaticIssueReviewReceipt {
    pub issue_id: String,
    pub revision: u64,
    pub run_id: String,
    pub full_commit_id: String,
    pub evidence_references: Vec<String>,
    pub lease_closed: bool,
}
impl From<rpc::IssueGatewayAwaitingReviewMutation> for AutomaticIssueReviewReceipt {
    fn from(value: rpc::IssueGatewayAwaitingReviewMutation) -> Self {
        Self {
            issue_id: value.issue_id,
            revision: value.revision,
            run_id: value.run_id,
            full_commit_id: value.full_commit_id,
            evidence_references: value.evidence_references,
            lease_closed: value.lease_closed,
        }
    }
}

#[derive(Debug, Clone, Copy, Error, Eq, PartialEq)]
pub enum AutomaticIssueGatewayError {
    #[error("automatic Issue gateway request is invalid")]
    InvalidRequest,
    #[error(
        "automatic Issue claim has already been requested; reconcile instead of claiming again"
    )]
    AlreadyRequested,
    #[error("automatic Issue gateway outcome is unconfirmed")]
    OutcomeUnknown,
    #[error("automatic Issue claim was rejected by its eligibility or revision policy")]
    Rejected,
}

/// Owns one automatic execution's claim dispatch, never a renderer-selected endpoint.
pub struct AutomaticIssueGateway {
    endpoint: String,
    claim_requested: Arc<AtomicBool>,
}

/// The Gateway holds the lease handle; its opaque capability stays process-local.
pub struct AutomaticIssueClaim {
    owner: Arc<AtomicBool>,
    run_id: String,
    issue_id: String,
    revision: AtomicU64,
    capability_id: String,
    review_requested: AtomicBool,
    mutations: Arc<tokio::sync::Mutex<()>>,
    unconfirmed: AtomicBool,
}

/// An admitted mutation owns its revision until its single result is observed.
pub struct PendingAutomaticIssueReview {
    endpoint: String,
    issue_id: String,
    capability_id: String,
    revision: u64,
    key: String,
    completion: issue::AwaitingReviewCompletion,
    _mutation: tokio::sync::OwnedMutexGuard<()>,
}

pub struct OwnedIssueRevision {
    run_id: String,
    issue_id: String,
    revision: u64,
    _mutation: tokio::sync::OwnedMutexGuard<()>,
}

/// Keeps the lease alive while its execution owns the automatic Issue.
pub struct ClaimedAutomaticIssue {
    gateway: Arc<AutomaticIssueGateway>,
    claim: Arc<AutomaticIssueClaim>,
    stop: tokio::sync::watch::Sender<bool>,
    result: tokio::sync::Mutex<ClaimedResult>,
}

#[derive(Default)]
struct ClaimedResult {
    cancelled: bool,
    admitted: bool,
    pending: Option<(String, Vec<u8>)>,
}

/// Stops new renewals if admission is rejected or its future is dropped.
pub struct LeaseRenewalGuard {
    stop: Option<tokio::sync::watch::Sender<bool>>,
}
impl LeaseRenewalGuard {
    pub fn accept(mut self) {
        self.stop = None;
    }
}
impl Drop for LeaseRenewalGuard {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(true);
        }
    }
}

impl ClaimedAutomaticIssue {
    pub fn renewal_guard(&self) -> LeaseRenewalGuard {
        LeaseRenewalGuard {
            stop: Some(self.stop.clone()),
        }
    }

    pub fn new(gateway: AutomaticIssueGateway, claim: AutomaticIssueClaim) -> Self {
        let gateway = Arc::new(gateway);
        let claim = Arc::new(claim);
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        tokio::spawn({
            let gateway = gateway.clone();
            let claim = claim.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = stopped.changed() => break,
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                    if *stopped.borrow() {
                        break;
                    }
                    // Finish an accepted mutation even if its owner stops during the RPC.
                    if gateway.heartbeat(&claim).await.is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            gateway,
            claim,
            stop,
            result: tokio::sync::Mutex::new(ClaimedResult::default()),
        }
    }

    pub async fn owned_revision(&self) -> Result<OwnedIssueRevision, AutomaticIssueGatewayError> {
        let state = self.result.lock().await;
        if state.cancelled || state.admitted {
            return Err(AutomaticIssueGatewayError::AlreadyRequested);
        }
        drop(state);
        self.gateway.owned_revision(&self.claim).await
    }

    pub async fn heartbeat(&self) -> Result<(), AutomaticIssueGatewayError> {
        let state = self.result.lock().await;
        if state.cancelled || state.admitted {
            return Err(AutomaticIssueGatewayError::AlreadyRequested);
        }
        drop(state);
        self.gateway.heartbeat(&self.claim).await
    }

    pub async fn admitted_revision(
        &self,
    ) -> Result<OwnedIssueRevision, AutomaticIssueGatewayError> {
        if !self.result.lock().await.admitted {
            return Err(AutomaticIssueGatewayError::InvalidRequest);
        }
        self.gateway.owned_revision(&self.claim).await
    }

    pub async fn cancel(&self) {
        self.result.lock().await.cancelled = true;
        let _ = self.stop.send(true);
    }

    /// Reserve locally so coordinator cancellation never waits for a tracker response.
    pub async fn admit_result(
        &self,
        run_id: &str,
        commit_id: &str,
        report: &[u8],
    ) -> Result<(), AutomaticIssueGatewayError> {
        if run_id != self.claim.run_id
            || !super::model::is_full_commit_id(commit_id)
            || report.len() > super::report::MAX_REPORT_BYTES
            || !serde_json::from_slice::<serde_json::Value>(report)
                .is_ok_and(|value| value.is_object())
        {
            return Err(AutomaticIssueGatewayError::InvalidRequest);
        }
        let mut state = self.result.lock().await;
        if state.cancelled || state.admitted {
            return Err(AutomaticIssueGatewayError::AlreadyRequested);
        }
        state.admitted = true;
        state.pending = Some((commit_id.into(), report.to_vec()));
        let _ = self.stop.send(true);
        Ok(())
    }

    pub async fn save_result(
        &self,
    ) -> Result<AutomaticIssueReviewReceipt, AutomaticIssueGatewayError> {
        let (commit, report) = self
            .result
            .lock()
            .await
            .pending
            .take()
            .ok_or(AutomaticIssueGatewayError::AlreadyRequested)?;
        self.gateway
            .complete_to_review(&self.claim, &self.claim.run_id, &commit, &report)
            .await
    }
}

impl Drop for ClaimedAutomaticIssue {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}
impl OwnedIssueRevision {
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn run_id(&self) -> &str {
        &self.run_id
    }
    pub fn issue_id(&self) -> &str {
        &self.issue_id
    }
}
impl PendingAutomaticIssueReview {
    pub async fn finish(self) -> Result<AutomaticIssueReviewReceipt, AutomaticIssueGatewayError> {
        IssueTrackerGrpcService::new(self.endpoint.clone())
            .complete_execution_to_awaiting_review(
                &self.endpoint,
                &self.issue_id,
                Some(&self.capability_id),
                self.revision,
                &self.key,
                self.completion,
            )
            .await
            .map(AutomaticIssueReviewReceipt::from)
            .map_err(|_| AutomaticIssueGatewayError::OutcomeUnknown)
    }
}
impl fmt::Debug for AutomaticIssueClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AutomaticIssueClaim")
            .field("run_id", &self.run_id)
            .field("issue_id", &self.issue_id)
            .field("revision", &self.revision())
            .finish_non_exhaustive()
    }
}
impl AutomaticIssueClaim {
    pub fn issue_id(&self) -> &str {
        &self.issue_id
    }
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }
}

impl AutomaticIssueGateway {
    pub fn new(endpoint: String) -> Result<Self, AutomaticIssueGatewayError> {
        let url =
            url::Url::parse(&endpoint).map_err(|_| AutomaticIssueGatewayError::InvalidRequest)?;
        if url.scheme() != "http"
            || !url
                .host_str()
                .is_some_and(|host| matches!(host, "127.0.0.1" | "[::1]" | "localhost"))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(AutomaticIssueGatewayError::InvalidRequest);
        }
        Ok(Self {
            endpoint,
            claim_requested: Arc::new(AtomicBool::new(false)),
        })
    }
    pub async fn claim(
        &self,
        run_id: &str,
        issue_id: &str,
        expected_revision: u64,
    ) -> Result<AutomaticIssueClaim, AutomaticIssueGatewayError> {
        if !valid_id(run_id) || !valid_id(issue_id) || expected_revision == u64::MAX {
            return Err(AutomaticIssueGatewayError::InvalidRequest);
        }
        self.claim_requested
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| AutomaticIssueGatewayError::AlreadyRequested)?;
        let channel = crate::grpc::connect(&self.endpoint)
            .await
            .map_err(|_| AutomaticIssueGatewayError::OutcomeUnknown)?;
        let mut client = rpc::issue_gateway_client::IssueGatewayClient::new(channel);
        let mut request = tonic::Request::new(rpc::IssueGatewayClaimRequest {
            issue_id: issue_id.into(),
            expected_revision: Some(expected_revision),
            idempotency_key: format!("coding-agent:{run_id}:{issue_id}:{expected_revision}:claim"),
        });
        request.set_timeout(Duration::from_secs(10));
        let response = client
            .claim(request)
            .await
            .map_err(|_| AutomaticIssueGatewayError::OutcomeUnknown)?
            .into_inner();
        match response.outcome {
            Some(rpc::issue_gateway_claim_response::Outcome::Success(claimed))
                if claimed.issue_id == issue_id
                    && claimed.revision == expected_revision + 1
                    && valid_id(&claimed.capability_id) =>
            {
                Ok(AutomaticIssueClaim {
                    owner: self.claim_requested.clone(),
                    run_id: run_id.into(),
                    issue_id: claimed.issue_id,
                    revision: AtomicU64::new(claimed.revision),
                    capability_id: claimed.capability_id,
                    review_requested: AtomicBool::new(false),
                    mutations: Arc::new(tokio::sync::Mutex::new(())),
                    unconfirmed: AtomicBool::new(false),
                })
            }
            Some(rpc::issue_gateway_claim_response::Outcome::Error(error))
                if matches!(
                    error.code.as_str(),
                    "precondition_failed" | "revision_conflict" | "idempotency_conflict"
                ) =>
            {
                Err(AutomaticIssueGatewayError::Rejected)
            }
            _ => Err(AutomaticIssueGatewayError::OutcomeUnknown),
        }
    }
    pub async fn heartbeat(
        &self,
        claim: &AutomaticIssueClaim,
    ) -> Result<(), AutomaticIssueGatewayError> {
        if !Arc::ptr_eq(&claim.owner, &self.claim_requested) {
            return Err(AutomaticIssueGatewayError::InvalidRequest);
        }
        let _mutation = claim.mutations.lock().await;
        if claim.unconfirmed.load(Ordering::SeqCst) {
            return Err(AutomaticIssueGatewayError::OutcomeUnknown);
        }
        if claim.review_requested.load(Ordering::SeqCst) {
            return Err(AutomaticIssueGatewayError::AlreadyRequested);
        }
        let revision = claim.revision();
        if revision == u64::MAX {
            return Err(AutomaticIssueGatewayError::InvalidRequest);
        }
        // Losing a mutation response must prevent any new mutation with a stale revision.
        claim.unconfirmed.store(true, Ordering::SeqCst);
        let channel = crate::grpc::connect(&self.endpoint)
            .await
            .map_err(|_| AutomaticIssueGatewayError::OutcomeUnknown)?;
        let mut client = rpc::issue_gateway_client::IssueGatewayClient::new(channel);
        let mut request = tonic::Request::new(rpc::IssueGatewayHeartbeatRequest {
            issue_id: claim.issue_id.clone(),
            capability_id: claim.capability_id.clone(),
            expected_revision: Some(revision),
            idempotency_key: format!(
                "coding-agent:{}:{}:{revision}:heartbeat",
                claim.run_id, claim.issue_id
            ),
        });
        request.set_timeout(Duration::from_secs(10));
        let response = client
            .heartbeat(request)
            .await
            .map_err(|_| AutomaticIssueGatewayError::OutcomeUnknown)?
            .into_inner();
        match response.outcome {
            Some(rpc::issue_gateway_mutation_response::Outcome::Success(updated))
                if updated.issue_id == claim.issue_id && updated.revision == revision + 1 =>
            {
                claim.revision.store(updated.revision, Ordering::SeqCst);
                claim.unconfirmed.store(false, Ordering::SeqCst);
                Ok(())
            }
            _ => Err(AutomaticIssueGatewayError::OutcomeUnknown),
        }
    }
    pub async fn owned_revision(
        &self,
        claim: &AutomaticIssueClaim,
    ) -> Result<OwnedIssueRevision, AutomaticIssueGatewayError> {
        if !Arc::ptr_eq(&claim.owner, &self.claim_requested) {
            return Err(AutomaticIssueGatewayError::InvalidRequest);
        }
        let mutation = claim.mutations.clone().lock_owned().await;
        if claim.unconfirmed.load(Ordering::SeqCst) {
            return Err(AutomaticIssueGatewayError::OutcomeUnknown);
        }
        if claim.review_requested.load(Ordering::SeqCst) {
            return Err(AutomaticIssueGatewayError::AlreadyRequested);
        }
        Ok(OwnedIssueRevision {
            run_id: claim.run_id.clone(),
            issue_id: claim.issue_id.clone(),
            revision: claim.revision(),
            _mutation: mutation,
        })
    }
    pub async fn complete_to_review(
        &self,
        claim: &AutomaticIssueClaim,
        run_id: &str,
        commit_id: &str,
        report_bytes: &[u8],
    ) -> Result<AutomaticIssueReviewReceipt, AutomaticIssueGatewayError> {
        self.begin_complete_to_review(claim, run_id, commit_id, report_bytes)
            .await?
            .finish()
            .await
    }

    pub async fn begin_complete_to_review(
        &self,
        claim: &AutomaticIssueClaim,
        run_id: &str,
        commit_id: &str,
        report_bytes: &[u8],
    ) -> Result<PendingAutomaticIssueReview, AutomaticIssueGatewayError> {
        if !Arc::ptr_eq(&claim.owner, &self.claim_requested)
            || claim.run_id != run_id
            || !super::model::is_full_commit_id(commit_id)
            || report_bytes.len() > super::report::MAX_REPORT_BYTES
        {
            return Err(AutomaticIssueGatewayError::InvalidRequest);
        }
        let mutation = claim.mutations.clone().lock_owned().await;
        if claim.unconfirmed.load(Ordering::SeqCst) {
            return Err(AutomaticIssueGatewayError::OutcomeUnknown);
        }
        let revision = claim.revision();
        let report: serde_json::Value = serde_json::from_slice(report_bytes)
            .map_err(|_| AutomaticIssueGatewayError::InvalidRequest)?;
        if !report.is_object() {
            return Err(AutomaticIssueGatewayError::InvalidRequest);
        }
        let text = serde_json::to_string(&report)
            .map_err(|_| AutomaticIssueGatewayError::InvalidRequest)?;
        // JSON permits C1 controls literally, but the tracker requires control-free evidence.
        let text = text.chars().fold(String::new(), |mut output, character| {
            if character.is_control() {
                use std::fmt::Write;
                let _ = write!(output, "\\u{:04x}", character as u32);
            } else {
                output.push(character);
            }
            output
        });
        let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        let reference = format!("urn:lookback:coding-agent:{run_id}:report:{digest}");
        let mut evidence = Vec::new();
        let mut remaining = text.as_str();
        while !remaining.is_empty() {
            let mut end = remaining.len().min(4090);
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            let (part, rest) = remaining.split_at(end);
            evidence.push(issue::CompletionEvidence {
                kind: issue::CompletionEvidenceKind::OtherEvidence as i32,
                summary: format!("{}:{part}", evidence.len() + 1),
                uri: None,
            });
            remaining = rest;
        }
        let completion = issue::AwaitingReviewCompletion {
            run_id: run_id.into(),
            full_commit_id: commit_id.into(),
            evidence_references: vec![reference],
            report: Some(issue::CompletionReport {
                result: Some(issue::completion_report::Result::Verified(
                    issue::VerifiedCompletion { evidence },
                )),
            }),
        };
        let key = format!(
            "coding-agent:{run_id}:review:{:x}",
            Sha256::digest(completion.encode_to_vec())
        );
        crate::grpc::issue_tracker::build_awaiting_review_request(
            &claim.issue_id,
            Some(&claim.capability_id),
            revision,
            &key,
            completion.clone(),
        )
        .map_err(|_| AutomaticIssueGatewayError::InvalidRequest)?;
        claim
            .review_requested
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| AutomaticIssueGatewayError::AlreadyRequested)?;
        Ok(PendingAutomaticIssueReview {
            endpoint: self.endpoint.clone(),
            issue_id: claim.issue_id.clone(),
            capability_id: claim.capability_id.clone(),
            revision,
            key,
            completion,
            _mutation: mutation,
        })
    }
}
fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}
