use std::{collections::HashMap, path::Path, sync::Arc};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::execution::{CancellationObservation, ExecutionResult};
use super::{
    AgentRunSnapshot, AllowedOperation, ChildWritersStatus, ExecutionAdapter, ExecutionEvent,
    ExecutionEventStream, ExecutionIdentity, ExecutionInvocationKind, HostFinalizationObservation,
    ImmutableExecutionIntent, InteractiveFinishObservation, OperationReconciliation,
    ProcessExitObservation, PromptDelivery, PublicationPlan, ReportExpectation, RunId, RunPurpose,
    RuntimeObservation, ShellProcessObservation, StartReceipt, TaskKind, ValidationEvidence,
    ValidationState, WorkflowDefinitionSnapshot, WorkspaceFreezeStatus, WorkspaceReceipt,
    WorkspaceRequest,
    model::ApprovalAssessmentContract,
    one_shot_workflow::{
        OneShotWorkflowExecution, OneShotWorkflowResult, VerifiedOneShotRuntime,
        verify_one_shot_runtime,
    },
    terminal::TerminalControl,
};
use crate::{
    jobworkerp::JobworkerpHandle,
    native_terminal::{
        launch::{NativeCliProfile, map_prepared_workspace},
        preparation::PreparedAgentWorkspace,
        registry::NativeRuntimeRegistry,
        wire::NativeTerminalPrepareRequest,
    },
};

mod reassessment;
use reassessment::SemanticTarget;
#[cfg(test)]
use reassessment::verify_saved_semantic_source;

#[derive(Default)]
struct DispatchClaim {
    claimed: bool,
    retired: bool,
}
impl DispatchClaim {
    fn retire_before_dispatch(&mut self) -> Result<(), ExecutionResult> {
        if self.claimed {
            return Err(ExecutionResult::ReconciliationUnknown);
        }
        self.retired = true;
        Ok(())
    }
    fn claim(&mut self) -> Result<(), ExecutionResult> {
        if self.retired {
            return Err(ExecutionResult::Conflict);
        }
        if self.claimed {
            return Err(ExecutionResult::ReconciliationUnknown);
        }
        self.claimed = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn semantic_recovery_can_be_cancelled_before_worker_observation_finishes() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn({
            let cancellation = cancellation.clone();
            async move {
                await_unless_cancelled(
                    &cancellation,
                    std::future::pending::<Result<(), ExecutionResult>>(),
                )
                .await
            }
        });
        cancellation.cancel();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap(),
            Err(ExecutionResult::Conflict)
        );
        assert_eq!(
            await_unless_cancelled(&cancellation, async { Ok(()) }).await,
            Err(ExecutionResult::Conflict)
        );
    }
    #[test]
    fn recovered_semantic_source_requires_the_original_issue_workspace() {
        let source = coding_agent_executor::PrepareRequest {
            issue_id: Some("42".into()),
            run_id: "run".into(),
            attempt_id: "attempt".into(),
            repository: coding_agent_executor::RepositoryId {
                host: "forge.example".into(),
                owner: "owner".into(),
                name: "repo".into(),
            },
            base_branch: "trunk".into(),
            base_commit: "a".repeat(40),
        };
        let receipt = WorkspaceReceipt {
            run_id: source.run_id.clone(),
            attempt_id: source.attempt_id.clone(),
            workspace_token: "/workspace/run".into(),
            repository_host: "forge.example".into(),
            repository_owner: "owner".into(),
            repository_name: "repo".into(),
            base_commit: source.base_commit.clone(),
            branch: source.expected_branch(),
        };
        assert!(verify_saved_semantic_source(&source, &receipt, "42").is_ok());
        for field in [
            "issue", "run", "attempt", "host", "owner", "repo", "base", "branch",
        ] {
            let mut wrong = source.clone();
            match field {
                "issue" => wrong.issue_id = Some("43".into()),
                "run" => wrong.run_id = "other".into(),
                "attempt" => wrong.attempt_id = "other".into(),
                "host" => wrong.repository.host = "other.example".into(),
                "owner" => wrong.repository.owner = "other".into(),
                "repo" => wrong.repository.name = "other".into(),
                "base" => wrong.base_commit = "b".repeat(40),
                "branch" => wrong.issue_id = None,
                _ => unreachable!(),
            }
            assert!(
                verify_saved_semantic_source(&wrong, &receipt, "42").is_err(),
                "{field}"
            );
        }
    }
    #[test]
    fn parallel_semantic_requests_keep_all_jobs_and_cancellation_tokens() {
        let mut attempt = Attempt::default();
        let first = tokio_util::sync::CancellationToken::new();
        let second = tokio_util::sync::CancellationToken::new();
        attempt.semantic_jobs.insert(
            "first".into(),
            ActiveSemanticJob {
                job_id: Some(71),
                cancellation: first.clone(),
            },
        );
        attempt.semantic_jobs.insert(
            "second".into(),
            ActiveSemanticJob {
                job_id: Some(72),
                cancellation: second.clone(),
            },
        );
        let mut jobs = attempt.cancel_semantic_jobs();
        jobs.sort_unstable();
        assert_eq!(jobs, vec![71, 72]);
        assert!(first.is_cancelled());
        assert!(second.is_cancelled());
        attempt.semantic_jobs.remove("second");
        assert_eq!(attempt.cancel_semantic_jobs(), vec![71]);
    }
    #[test]
    fn human_semantic_reassessment_requests_do_not_reauthorize_the_original_dispatch() {
        let mut attempt = Attempt::default();
        attempt.claim_semantic_request(None).unwrap();
        assert!(attempt.claim_semantic_request(None).is_err());
        attempt
            .claim_semantic_request(Some("human-request-1"))
            .unwrap();
        assert!(
            attempt
                .claim_semantic_request(Some("human-request-1"))
                .is_err()
        );
        attempt
            .claim_semantic_request(Some("human-request-2"))
            .unwrap();
        attempt.cancelled = true;
        assert!(
            attempt
                .claim_semantic_request(Some("human-request-3"))
                .is_err()
        );
    }
    #[test]
    fn publication_recovery_rejects_endpoint_changes_before_remote_lookup() {
        let repository = coding_agent_executor::RepositoryId {
            host: "forge.example".into(),
            owner: "owner".into(),
            name: "repo".into(),
        };
        let mut configured = super::super::executor_client::NativeRepositoryPublicationConfig {
            allowed_base_branches: Default::default(),
            forge_kind: Some(coding_agent_executor::ForgeKind::GitHub),
            web_base_url: Some("https://forge.example".into()),
            api_base_url: Some("https://forge.example/api".into()),
            clone_url: "ssh://git@forge.example/owner/repo.git".into(),
            read_credential_reference: None,
            publication_credential_reference: None,
        };
        assert!(publication_endpoints_match(
            &repository,
            "https://forge.example/api",
            "ssh://git@forge.example/owner/repo.git",
            "https://forge.example",
            true,
            &configured
        ));
        assert!(publication_endpoints_match(
            &repository,
            "https://forge.example/api",
            "ssh://git@forge.example/owner/repo",
            "https://forge.example",
            true,
            &configured
        ));
        for url in [
            "ssh://other@forge.example/owner/repo.git",
            "ssh://git@forge.example:2222/owner/repo.git",
            "https://forge.example/owner/repo.git",
            "ssh://git@forge.example/owner/other.git",
        ] {
            assert!(!publication_endpoints_match(
                &repository,
                "https://forge.example/api",
                url,
                "https://forge.example",
                true,
                &configured
            ));
        }
        for field in ["api", "clone", "web"] {
            let original = configured.clone();
            match field {
                "api" => configured.api_base_url = Some("https://other.example/api".into()),
                "clone" => configured.clone_url = "ssh://git@other.example/owner/repo.git".into(),
                "web" => configured.web_base_url = Some("https://other.example".into()),
                _ => unreachable!(),
            }
            assert!(!publication_endpoints_match(
                &repository,
                "https://forge.example/api",
                "ssh://git@forge.example/owner/repo.git",
                "https://forge.example",
                true,
                &configured
            ));
            configured = original;
        }
    }
    #[test]
    fn publication_policy_matches_the_approved_forge_and_credential_references() {
        let mut configured = super::super::executor_client::NativeRepositoryPublicationConfig {
            clone_url: "ssh://git@forge.example/owner/repo.git".into(),
            api_base_url: Some("https://forge.example/api".into()),
            allowed_base_branches: Default::default(),
            forge_kind: Some(coding_agent_executor::ForgeKind::GitHub),
            web_base_url: Some("https://forge.example".into()),
            read_credential_reference: Some("read".into()),
            publication_credential_reference: Some("publish".into()),
        };
        let forge = super::super::repository::ForgeKind::GitHub;
        assert!(publication_policy_matches(
            &forge,
            Some("read"),
            Some("publish"),
            &configured
        ));
        assert!(!publication_policy_matches(
            &forge,
            None,
            Some("publish"),
            &configured
        ));
        assert!(!publication_policy_matches(
            &forge,
            Some("read"),
            Some("other"),
            &configured
        ));
        configured.forge_kind = Some(coding_agent_executor::ForgeKind::Gitea);
        assert!(!publication_policy_matches(
            &forge,
            Some("read"),
            Some("publish"),
            &configured
        ));
        configured.forge_kind = None;
        assert!(publication_policy_matches(
            &forge,
            Some("read"),
            Some("publish"),
            &configured
        ));
    }
    #[tokio::test]
    async fn publication_admission_can_be_cancelled_before_the_next_remote_phase() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let pending = tokio::spawn({
            let cancellation = cancellation.clone();
            async move {
                await_unless_cancelled(
                    &cancellation,
                    std::future::pending::<Result<(), ExecutionResult>>(),
                )
                .await
            }
        });
        cancellation.cancel();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap(),
            Err(ExecutionResult::Conflict)
        );
        assert_eq!(
            await_unless_cancelled(&cancellation, async { Ok(()) }).await,
            Err(ExecutionResult::Conflict)
        );
        assert_eq!(
            await_unless_cancelled(&tokio_util::sync::CancellationToken::new(), async {
                Ok(81)
            })
            .await,
            Ok(81)
        );
    }

    #[test]
    fn definitive_publication_rejections_are_distinct_from_unconfirmed_remote_effects() {
        use super::super::NativeExecutorError;
        assert_eq!(
            publication_failure(NativeExecutorError::ExecutorRejected(
                coding_agent_executor::ExecutorErrorCode::RemoteBranchConflict
            )),
            super::super::PublicationOutcome::Rejected
        );
        for error in [
            NativeExecutorError::Unavailable,
            NativeExecutorError::OutcomeUnknown {
                operation: "push_workspace",
            },
        ] {
            assert_eq!(
                publication_failure(error),
                super::super::PublicationOutcome::Unknown
            );
        }
    }

    #[test]
    fn workflow_publication_source_is_bound_to_the_frozen_issue_workspace() {
        let prepared = coding_agent_executor::PrepareRequest {
            issue_id: Some("42".into()),
            run_id: "run".into(),
            attempt_id: "attempt".into(),
            repository: coding_agent_executor::RepositoryId {
                host: "forge.example".into(),
                owner: "owner".into(),
                name: "repo".into(),
            },
            base_branch: "main".into(),
            base_commit: "a".repeat(40),
        };
        let request = super::super::PublicationRequest {
            run_id: prepared.run_id.clone(),
            attempt_id: prepared.attempt_id.clone(),
            repository_host: "forge.example".into(),
            repository_owner: "owner".into(),
            repository_name: "repo".into(),
            approved_web_base_url: "https://forge.example".into(),
            branch: prepared.expected_branch(),
            base_branch: prepared.base_branch.clone(),
            base_commit: prepared.base_commit.clone(),
            frozen_commit: "b".repeat(40),
            plan: PublicationPlan::PushBranch,
            operation_key: "operation".into(),
            pull_request_title: None,
            pull_request_body: None,
        };
        let persisted = coding_agent_executor::WorkspacePushRequest {
            source: prepared.clone(),
            expected_commit: request.frozen_commit.clone(),
            source_digest: "e".repeat(64),
        };
        assert!(publication_lookup_request(&request, &persisted).is_ok());
        for field in ["attempt", "repo", "base", "branch", "commit", "digest"] {
            let mut changed = persisted.clone();
            match field {
                "attempt" => changed.source.attempt_id = "other".into(),
                "repo" => changed.source.repository.name = "other".into(),
                "base" => changed.source.base_branch = "trunk".into(),
                "branch" => changed.source.issue_id = Some("999".into()),
                "commit" => changed.expected_commit = "c".repeat(40),
                "digest" => changed.source_digest.clear(),
                _ => unreachable!(),
            }
            assert!(
                publication_lookup_request(&request, &changed).is_err(),
                "{field}"
            );
        }
        let source = coding_agent_executor::SourceInspection {
            run_id: prepared.run_id.clone(),
            attempt_id: prepared.attempt_id.clone(),
            base_commit: prepared.base_commit.clone(),
            head_commit: request.frozen_commit.clone(),
            branch: request.branch.clone(),
            tracked_diff: b"actual diff".to_vec(),
            uncommitted_diff: Some(Vec::new()),
            new_files: Vec::new(),
        };
        assert!(
            validate_publication_source(&prepared, &PublicationPlan::PushBranch, &request, &source)
                .is_ok()
        );
        for field in [
            "run",
            "attempt",
            "repository",
            "branch",
            "base",
            "commit",
            "plan",
        ] {
            let mut changed = request.clone();
            match field {
                "run" => changed.run_id = "other".into(),
                "attempt" => changed.attempt_id = "other".into(),
                "repository" => changed.repository_owner = "other".into(),
                "branch" => changed.branch = "main".into(),
                "base" => changed.base_commit = "c".repeat(40),
                "commit" => changed.frozen_commit = "d".repeat(40),
                "plan" => changed.plan = PublicationPlan::CreatePullRequest,
                _ => unreachable!(),
            }
            assert!(
                validate_publication_source(
                    &prepared,
                    &PublicationPlan::PushBranch,
                    &changed,
                    &source
                )
                .is_err(),
                "{field}"
            );
        }
        for changes in [None, Some(vec![1])] {
            let mut dirty = source.clone();
            dirty.uncommitted_diff = changes;
            assert!(
                validate_publication_source(
                    &prepared,
                    &PublicationPlan::PushBranch,
                    &request,
                    &dirty
                )
                .is_err()
            );
        }
        let mut unchanged = source.clone();
        unchanged.tracked_diff.clear();
        let skip = publication_skip_outcome(&unchanged).unwrap();
        assert!(matches!(
            skip,
            super::super::PublicationOutcome::NoSourceChanges { .. }
        ));
        assert!(publication_skip_outcome(&source).is_none());
        unchanged.uncommitted_diff = Some(vec![1]);
        assert!(publication_skip_outcome(&unchanged).is_none());
    }
    #[test]
    fn stale_unstarted_candidates_can_be_retired_without_reauthorizing_claimed_attempts() {
        let mut stale = DispatchClaim::default();
        stale.retire_before_dispatch().unwrap();
        assert_eq!(stale.claim(), Err(ExecutionResult::Conflict));
        let mut claimed = DispatchClaim::default();
        claimed.claim().unwrap();
        assert_eq!(
            claimed.retire_before_dispatch(),
            Err(ExecutionResult::ReconciliationUnknown)
        );
        assert_eq!(claimed.claim(), Err(ExecutionResult::ReconciliationUnknown));
    }
    #[test]
    fn an_accepted_or_unconfirmed_dispatch_is_never_claimed_twice() {
        let mut claim = DispatchClaim::default();
        assert!(claim.claim().is_ok());
        assert_eq!(claim.claim(), Err(ExecutionResult::ReconciliationUnknown));
        assert_eq!(claim.claim(), Err(ExecutionResult::ReconciliationUnknown));
    }

    #[test]
    fn finished_validation_jobs_are_retired_before_cancelling_another_phase() {
        let mut attempt = Attempt {
            validation_jobs: vec![71, 72],
            ..Default::default()
        };
        attempt.validation_finished(Some(71));
        assert_eq!(attempt.validation_jobs, vec![72]);
        attempt.validation_finished(None);
        assert_eq!(attempt.validation_jobs, vec![72]);
        attempt.validation_finished(Some(72));
        assert!(attempt.validation_jobs.is_empty());
    }

    #[test]
    fn an_old_helper_cannot_admit_a_commit_that_it_cannot_observe() {
        assert_eq!(
            require_commit_observation_capability(None),
            Err(ExecutionResult::Unsupported)
        );
        assert!(require_commit_observation_capability(Some(&[])).is_ok());
        assert!(require_commit_observation_capability(Some(b"actual diff")).is_ok());
    }
}

pub fn prepared_snapshot_digest(
    key: &str,
    snapshot: &AgentRunSnapshot,
) -> Result<String, ExecutionResult> {
    let bytes = serde_json::to_vec(&(key, snapshot)).map_err(|_| ExecutionResult::Conflict)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub fn verify_prepared_approval(
    key: &str,
    snapshot: &AgentRunSnapshot,
    expected: &str,
) -> Result<(), ExecutionResult> {
    if prepared_snapshot_digest(key, snapshot)? != expected {
        return Err(ExecutionResult::Conflict);
    }
    Ok(())
}

/// App-owned automatic execution. Prepared candidates cannot be reconstructed
/// from renderer data or used as permission to redispatch after a restart.
pub struct NativeExecutionAdapter {
    registry: Arc<NativeRuntimeRegistry>,
    candidates: Mutex<HashMap<String, Arc<Candidate>>>,
    plans: Mutex<HashMap<String, String>>,
    planning: Mutex<()>,
    llm_settings_gate: Arc<Mutex<()>>,
    judge_activity: Arc<super::semantic_judge::JudgeActivity>,
    publication_store: Option<Arc<super::RunStore>>,
    recovered_semantics: Mutex<HashMap<String, Arc<SemanticTarget>>>,
    recovered_cancellations: Mutex<HashMap<String, tokio_util::sync::CancellationToken>>,
}

struct Candidate {
    identity: std::sync::OnceLock<ExecutionIdentity>,
    publication_cancellation: tokio_util::sync::CancellationToken,
    snapshot: AgentRunSnapshot,
    contract: ApprovalAssessmentContract,
    semantic: Option<SemanticCandidate>,
    prepared: PreparedAgentWorkspace,
    handle: JobworkerpHandle,
    verified: VerifiedOneShotRuntime,
    source_digest: String,
    generation: u64,
    cwd: String,
    prompt: Vec<u8>,
    expectation: ReportExpectation,
    observation: RuntimeObservation,
    execution: Arc<Mutex<Attempt>>,
    executor: crate::native_terminal::bootstrap::NativeWorkspaceExecutor,
}

struct SemanticCandidate {
    settings: crate::commands::llm_settings::LlmSettings,
    judge: Arc<super::semantic_judge::RegisteredSemanticJudge>,
}

#[derive(Default)]
struct Attempt {
    dispatch: DispatchClaim,
    intent: Option<ImmutableExecutionIntent>,
    receipt: Option<StartReceipt>,
    execution: Option<OneShotWorkflowExecution>,
    result: Option<OneShotWorkflowResult>,
    commit_result: Option<OneShotWorkflowResult>,
    commit_dispatch: DispatchClaim,
    commit_execution: Option<OneShotWorkflowExecution>,
    commit_parent_job_id: Option<i64>,
    validation_dispatch: HashMap<String, DispatchClaim>,
    validation_executions: HashMap<String, OneShotWorkflowExecution>,
    validation_jobs: Vec<i64>,
    cancelled: bool,
    semantic_dispatch: DispatchClaim,
    semantic_reassessments: HashMap<String, DispatchClaim>,
    semantic_jobs: HashMap<String, ActiveSemanticJob>,
    issue_claim: Option<Arc<super::issue_gateway::ClaimedAutomaticIssue>>,
    publication_dispatch: DispatchClaim,
    publication_request: Option<super::PublicationRequest>,
    publication_execution: Option<super::executor_client::PendingWorkspacePush>,
    publication_jobs: Vec<i64>,
    publication_outcome: Option<super::PublicationOutcome>,
}

struct ActiveSemanticJob {
    job_id: Option<i64>,
    cancellation: tokio_util::sync::CancellationToken,
}

impl Attempt {
    fn cancel_semantic_jobs(&self) -> Vec<i64> {
        self.semantic_jobs
            .values()
            .filter_map(|job| {
                job.cancellation.cancel();
                job.job_id
            })
            .collect()
    }

    fn claim_semantic_request(&mut self, request_key: Option<&str>) -> Result<(), ExecutionResult> {
        if self.cancelled {
            return Err(ExecutionResult::Conflict);
        }
        match request_key {
            Some(key) => self
                .semantic_reassessments
                .entry(key.to_owned())
                .or_default()
                .claim(),
            None => self.semantic_dispatch.claim(),
        }
    }
}

impl Attempt {
    fn validation_finished(&mut self, parent_job_id: Option<i64>) {
        if let Some(parent_job_id) = parent_job_id {
            self.validation_jobs.retain(|id| *id != parent_job_id);
        }
    }
}

impl NativeExecutionAdapter {
    pub fn with_publication_store(mut self, store: Arc<super::RunStore>) -> Self {
        self.publication_store = Some(store);
        self
    }

    pub fn new(registry: Arc<NativeRuntimeRegistry>, llm_settings_gate: Arc<Mutex<()>>) -> Self {
        Self {
            registry,
            candidates: Mutex::new(HashMap::new()),
            plans: Mutex::new(HashMap::new()),
            planning: Mutex::new(()),
            llm_settings_gate,
            judge_activity: Arc::default(),
            publication_store: None,
            recovered_semantics: Mutex::new(HashMap::new()),
            recovered_cancellations: Mutex::new(HashMap::new()),
        }
    }

    pub fn has_active_semantic_judges(&self) -> bool {
        self.judge_activity.is_active()
    }

    pub async fn preview_candidate(
        &self,
        key: &str,
        request: NativeTerminalPrepareRequest,
        semantic_judge_enabled: bool,
        publication_plan: PublicationPlan,
        contract: &ApprovalAssessmentContract,
        manual_repository: Option<super::ResolvedRepository>,
    ) -> Result<AgentRunSnapshot, ExecutionResult> {
        let _planning = self.planning.lock().await;
        let cached_run_id = { self.plans.lock().await.get(key).cloned() };
        if let Some(run_id) = cached_run_id {
            let candidate = self.by_id(&run_id).await?;
            if self.preflight(&candidate.snapshot).await.is_ok() {
                return Ok(candidate.snapshot.clone());
            }
            candidate
                .execution
                .lock()
                .await
                .dispatch
                .retire_before_dispatch()?;
            self.plans.lock().await.remove(key);
            self.candidates.lock().await.remove(&run_id);
        }
        let snapshot = self
            .prepare_candidate(
                request,
                contract.purpose,
                semantic_judge_enabled,
                publication_plan,
                contract,
                manual_repository,
            )
            .await?;
        self.plans
            .lock()
            .await
            .insert(key.into(), snapshot.run_id.to_string());
        Ok(snapshot)
    }

    pub async fn approved_candidate(
        &self,
        key: &str,
        expected_digest: &str,
    ) -> Result<AgentRunSnapshot, ExecutionResult> {
        let run_id = self
            .plans
            .lock()
            .await
            .get(key)
            .cloned()
            .ok_or(ExecutionResult::Conflict)?;
        let candidate = self.by_id(&run_id).await?;
        verify_prepared_approval(key, &candidate.snapshot, expected_digest)?;
        Ok(candidate.snapshot.clone())
    }

    pub async fn prepare_candidate(
        &self,
        request: NativeTerminalPrepareRequest,
        purpose: RunPurpose,
        semantic_judge_enabled: bool,
        publication_plan: PublicationPlan,
        contract: &ApprovalAssessmentContract,
        manual_repository: Option<super::ResolvedRepository>,
    ) -> Result<AgentRunSnapshot, ExecutionResult> {
        if contract.purpose != purpose {
            return Err(ExecutionResult::Conflict);
        }
        let edited_prompt = request.prompt.clone();
        let (prepared, handle, source, executor) = self
            .registry
            .prepare_automatic_with_source(request, manual_repository)
            .await
            .map_err(|_| ExecutionResult::AdapterUnavailable)?;
        let inspection = executor
            .inspect_source(prepared_source_request(&prepared))
            .await
            .map_err(|_| ExecutionResult::AdapterUnavailable)?;
        require_commit_observation_capability(inspection.uncommitted_diff.as_deref())?;
        let profile = &prepared.authority.profile;
        let cli = NativeCliProfile::from_command_and_image(&profile.command, &profile.image_digest)
            .ok_or(ExecutionResult::Unsupported)?;
        let cwd = map_prepared_workspace(
            Path::new(&prepared.workspace.workspace_path),
            &prepared.mount_binding.canonical_host_root,
            &prepared.mount_binding.guest_root,
        )
        .map_err(|_| ExecutionResult::Conflict)?;
        let verified = verify_one_shot_runtime(&handle, &prepared.worker, &cli, &cwd)
            .await
            .map_err(|_| ExecutionResult::AdapterUnavailable)?;
        self.registry
            .verify_automatic_source(source.child_generation_id, &source.source_digest)
            .await
            .map_err(|_| ExecutionResult::Conflict)?;
        let workflow = verified.workflow();
        let mut snapshot = AgentRunSnapshot {
            run_id: RunId::new(&prepared.workspace.run_id)
                .map_err(|_| ExecutionResult::Conflict)?,
            issue_id: prepared.authority.canonical_issue.issue_id().into(),
            issue_revision: prepared.authority.canonical_issue.revision(),
            execution_mode: super::ExecutionMode::OneShot,
            purpose,
            repository: prepared.authority.repository.clone(),
            profile: profile.clone(),
            base_commit: prepared.workspace.base_commit.clone(),
            workspace_branch: prepared.workspace.branch.clone(),
            semantic_judge_enabled,
            auto_done: false,
            approval: None,
            allowed_operations: match publication_plan {
                PublicationPlan::NoPublication => vec![AllowedOperation::Verify],
                PublicationPlan::PushBranch => {
                    vec![AllowedOperation::Verify, AllowedOperation::PublishBranch]
                }
                PublicationPlan::CreatePullRequest => vec![
                    AllowedOperation::Verify,
                    AllowedOperation::PublishBranch,
                    AllowedOperation::CreatePullRequest,
                ],
            },
            publication_plan,
            workflow: WorkflowDefinitionSnapshot {
                definition_hash: workflow.definition_hash().into(),
                semantic_judge_worker_digest: None,
                template_id: Some(workflow.worker_name().into()),
                materialized_definition_hash: Some(workflow.definition_hash().into()),
                registered_worker_id: Some(workflow.worker_id().to_string()),
                workflow_execution_id: None,
                parent_job_id: None,
                child_sandbox_job_id: None,
            },
            attempt_ids: vec![prepared.workspace.attempt_id.clone()],
        };
        let expectation = ReportExpectation::new(
            snapshot.run_id.to_string(),
            snapshot.primary_attempt_id(),
            match purpose {
                RunPurpose::Implementation => TaskKind::Implementation,
                RunPurpose::Investigation => TaskKind::Investigation,
            },
            contract
                .criteria
                .iter()
                .map(|item| (item.criterion_id.clone(), item.required))
                .collect(),
            contract
                .validation_plan
                .iter()
                .map(|item| (item.validation_id.clone(), item.required))
                .collect(),
        )
        .map_err(|_| ExecutionResult::Conflict)?;
        let prompt = automatic_prompt(&prepared, edited_prompt.as_deref(), contract, &expectation)?;
        let (arguments_schema_sha256, result_schema_sha256) = workflow.schema_digests();
        let observation = RuntimeObservation {
            runtime_instance_id: source.child_generation_id.to_string(),
            health_observation_id: format!("cli-probe:{}", verified.probe_job_id()),
            worker_name: workflow.worker_name().into(),
            worker_id: workflow.worker_id(),
            runner_name: "WORKFLOW".into(),
            method_name: "run".into(),
            arguments_schema_sha256,
            result_schema_sha256,
            response_mode: "DIRECT".into(),
            output_is_streaming: true,
            accepts_client_stream: false,
            image_digest: prepared.worker.scope().image_digest().into(),
            admitted_resources: profile.resources,
            host_limits: profile.validated_host_limits,
            helper_version: String::new(),
            agent_cli_version: verified.cli_version().into(),
            helper_protocol_version: 0,
            helper_features: Default::default(),
            workflow_execution_id: String::new(),
            parent_job_id: String::new(),
            child_sandbox_job_id: String::new(),
        };
        let semantic = if semantic_judge_enabled {
            let _gate = self.llm_settings_gate.lock().await;
            let settings = self.registry.llm_settings_snapshot();
            if contract.provider.as_ref()
                != Some(&super::semantic_judge::provider_selection(&settings)?)
            {
                return Err(ExecutionResult::Conflict);
            }
            Some(SemanticCandidate {
                judge: Arc::new(
                    super::semantic_judge::RegisteredSemanticJudge::observe_for_settings(
                        &handle, &settings,
                    )
                    .await?
                    .with_policy(
                        self.registry
                            .semantic_judge_policy()
                            .map_err(|_| ExecutionResult::Conflict)?,
                    )?,
                ),
                settings,
            })
        } else {
            None
        };
        snapshot.workflow.semantic_judge_worker_digest = semantic
            .as_ref()
            .map(|candidate| candidate.judge.configuration_digest());
        let candidate = Candidate {
            identity: std::sync::OnceLock::new(),
            publication_cancellation: tokio_util::sync::CancellationToken::new(),
            snapshot: snapshot.clone(),
            contract: contract.clone(),
            semantic,
            prepared,
            handle,
            verified,
            source_digest: source.source_digest,
            generation: source.child_generation_id,
            cwd,
            prompt,
            expectation,
            observation,
            executor,
            execution: Arc::new(Mutex::new(Attempt::default())),
        };
        self.candidates
            .lock()
            .await
            .insert(snapshot.run_id.to_string(), Arc::new(candidate));
        Ok(snapshot)
    }

    pub async fn prepared_prompt(
        &self,
        snapshot: &AgentRunSnapshot,
    ) -> Result<Vec<u8>, ExecutionResult> {
        Ok(self.candidate(snapshot).await?.prompt.clone())
    }

    pub async fn verify_reassessment_issue(
        &self,
        snapshot: &AgentRunSnapshot,
        issue: &crate::commands::issues::IssueListItem,
    ) -> Result<(), ExecutionResult> {
        let candidate = match self.candidate(snapshot).await {
            Ok(candidate) => candidate,
            Err(ExecutionResult::ReconciliationUnknown) => {
                return self.verify_saved_reassessment_issue(snapshot, issue);
            }
            Err(error) => return Err(error),
        };
        let original = &candidate.prepared.authority.canonical_issue;
        if original.issue_id() != issue.id
            || original.title() != issue.title
            || original.description() != issue.description
            || candidate
                .contract
                .criteria
                .iter()
                .map(|item| &item.statement)
                .ne(issue.acceptance_criteria_items.iter())
        {
            return Err(ExecutionResult::Conflict);
        }
        Ok(())
    }

    async fn candidate(
        &self,
        snapshot: &AgentRunSnapshot,
    ) -> Result<Arc<Candidate>, ExecutionResult> {
        let candidate = self.by_id(snapshot.run_id.as_str()).await?;
        let mut expected = candidate.snapshot.clone();
        let mut actual = snapshot.clone();
        // Explicit approval is added after preview; it cannot alter execution inputs.
        expected.approval = None;
        actual.approval = None;
        expected.auto_done = actual.auto_done;
        if expected != actual {
            return Err(ExecutionResult::Conflict);
        }
        Ok(candidate)
    }

    async fn by_id(&self, run_id: &str) -> Result<Arc<Candidate>, ExecutionResult> {
        self.candidates
            .lock()
            .await
            .get(run_id)
            .cloned()
            .ok_or(ExecutionResult::ReconciliationUnknown)
    }

    async fn identity_candidate(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<Arc<Candidate>, ExecutionResult> {
        let candidate = self.by_id(&identity.run_id).await?;
        if candidate.identity.get() != Some(identity) {
            return Err(ExecutionResult::Conflict);
        }
        Ok(candidate)
    }
}

impl Candidate {
    async fn stop_issue_lease(&self) {
        let claim = self.execution.lock().await.issue_claim.clone();
        if let Some(claim) = claim {
            claim.cancel().await;
        }
    }

    fn workspace_receipt(&self) -> WorkspaceReceipt {
        let workspace = &self.prepared.workspace;
        WorkspaceReceipt {
            run_id: workspace.run_id.clone(),
            attempt_id: workspace.attempt_id.clone(),
            workspace_token: workspace.workspace_path.clone(),
            repository_host: workspace.repository.host.clone(),
            repository_owner: workspace.repository.owner.clone(),
            repository_name: workspace.repository.name.clone(),
            base_commit: workspace.base_commit.clone(),
            branch: workspace.branch.clone(),
        }
    }
}

impl NativeExecutionAdapter {
    async fn evaluate_semantics(
        &self,
        identity: &ExecutionIdentity,
        source: Option<&coding_agent_executor::SourceInspection>,
        report: &[u8],
        validation: &ValidationEvidence,
        request_key: Option<&str>,
    ) -> Result<super::semantic_judge::SemanticAssessment, ExecutionResult> {
        use super::semantic_judge::{
            SYSTEM_POLICY, SemanticAssessment, parse_response, prepare_input,
        };
        let warm = match self.identity_candidate(identity).await {
            Ok(candidate) => Some(candidate),
            Err(ExecutionResult::ReconciliationUnknown) if request_key.is_some() => None,
            Err(error) => return Err(error),
        };
        let candidate = self.semantic_target(identity, warm.as_ref()).await?;
        let semantic = &candidate.semantic;
        let source = source.ok_or(ExecutionResult::FinalizationUnavailable)?;
        if source.run_id != identity.run_id
            || source.attempt_id != identity.attempt_id
            || source.base_commit != candidate.snapshot.base_commit
            || source.branch != candidate.snapshot.workspace_branch
        {
            return Err(ExecutionResult::Conflict);
        }
        let input = match prepare_input(
            &candidate.snapshot.issue_id,
            &candidate.issue_title,
            &candidate.issue_description,
            &candidate.contract,
            source,
            report,
            validation,
        ) {
            Ok(input) => input,
            Err(_) => {
                return Ok(SemanticAssessment::inconclusive(
                    "approved_input_unavailable",
                ));
            }
        };
        let gate = await_unless_cancelled(&candidate.cancellation, async {
            Ok(self.llm_settings_gate.lock().await)
        })
        .await?;
        let _activity = self.judge_activity.enter();
        if candidate.cancellation.is_cancelled() {
            return Err(ExecutionResult::Conflict);
        }
        if warm.is_none() {
            self.recovered_semantic_run(identity)?;
        }
        if self.registry.llm_settings_snapshot() != semantic.settings
            || self
                .registry
                .semantic_judge_policy()
                .map_err(|_| ExecutionResult::Conflict)?
                != semantic.judge.policy()
        {
            return Ok(SemanticAssessment::inconclusive("judge_settings_changed"));
        }
        let cancellation = candidate.cancellation.child_token();
        let key = match request_key {
            Some(key) => format!(
                "{}:{}:semantic:human:{:x}",
                identity.run_id,
                identity.attempt_id,
                Sha256::digest(key.as_bytes())
            ),
            None => format!("{}:{}:semantic", identity.run_id, identity.attempt_id),
        };
        let claim = {
            let mut attempt = candidate.execution.lock().await;
            if attempt.cancelled {
                return Err(ExecutionResult::Conflict);
            }
            attempt.claim_semantic_request(request_key)?;
            attempt.semantic_jobs.insert(
                key.clone(),
                ActiveSemanticJob {
                    job_id: None,
                    cancellation: cancellation.clone(),
                },
            );
            if request_key.is_some() {
                None
            } else {
                Some(
                    attempt
                        .issue_claim
                        .clone()
                        .ok_or(ExecutionResult::ReconciliationUnknown)?,
                )
            }
        };
        if let Some(claim) = claim {
            let owned = claim
                .owned_revision()
                .await
                .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
            self.registry
                .verify_automatic_owned_authority(
                    &warm.as_ref().ok_or(ExecutionResult::Conflict)?.prepared,
                    &owned,
                )
                .await
                .map_err(|_| ExecutionResult::Conflict)?;
        }
        let execution = semantic
            .judge
            .begin_cancellable(
                &candidate.handle,
                SYSTEM_POLICY.as_bytes(),
                &input,
                &key,
                cancellation,
            )
            .await?;
        {
            let mut attempt = candidate.execution.lock().await;
            if attempt.cancelled {
                return Err(ExecutionResult::Conflict);
            }
            attempt
                .semantic_jobs
                .get_mut(&key)
                .ok_or(ExecutionResult::Conflict)?
                .job_id = execution.job_id();
        }
        drop(gate);
        let response = execution.finish().await?;
        let mut assessment = parse_response(&response.text, &input, &candidate.contract)
            .unwrap_or_else(|_| SemanticAssessment::inconclusive("invalid_judge_response"));
        assessment.input_sha256 = Some(format!("{:x}", Sha256::digest(&input)));
        assessment.worker_id = Some(response.worker_id);
        assessment.job_id = response.job_id;
        assessment.worker_configuration_digest = Some(semantic.judge.configuration_digest());
        let validated = super::validate_report(report, &candidate.expectation)
            .map_err(|_| ExecutionResult::Conflict)?;
        if assessment.outcome == super::assessment::AssessmentUse::Pass
            && (validated.report.status != super::AgentReportStatus::Completed
                || validated
                    .report
                    .unresolved
                    .iter()
                    .any(|item| item.blocks_completion))
        {
            assessment.outcome = super::assessment::AssessmentUse::Inconclusive;
            assessment.reason = "report_incomplete".into();
        }
        let mut attempt = candidate.execution.lock().await;
        if attempt.cancelled {
            return Err(ExecutionResult::Conflict);
        }
        attempt.semantic_jobs.remove(&key);
        Ok(assessment)
    }
}

#[async_trait]
impl ExecutionAdapter for NativeExecutionAdapter {
    async fn preflight(
        &self,
        snapshot: &AgentRunSnapshot,
    ) -> Result<RuntimeObservation, ExecutionResult> {
        let candidate = self.candidate(snapshot).await?;
        if let Some(semantic) = &candidate.semantic {
            let _gate = self.llm_settings_gate.lock().await;
            if semantic.settings != self.registry.llm_settings_snapshot()
                || self
                    .registry
                    .semantic_judge_policy()
                    .map_err(|_| ExecutionResult::Conflict)?
                    != semantic.judge.policy()
            {
                return Err(ExecutionResult::Conflict);
            }
            semantic.judge.revalidate(&candidate.handle).await?;
        }
        if !candidate.execution.lock().await.dispatch.claimed {
            self.registry
                .verify_automatic_authority(&candidate.prepared)
                .await
                .map_err(|_| ExecutionResult::Conflict)?;
        }
        self.registry
            .verify_automatic_source(candidate.generation, &candidate.source_digest)
            .await
            .map_err(|_| ExecutionResult::Conflict)?;
        candidate
            .verified
            .workflow()
            .revalidate(&candidate.handle)
            .await
            .map_err(|_| ExecutionResult::Conflict)?;
        Ok(candidate.observation.clone())
    }
    async fn prepare_workspace(
        &self,
        request: &WorkspaceRequest,
    ) -> Result<WorkspaceReceipt, ExecutionResult> {
        let candidate = self.candidate(&request.snapshot).await?;
        if request.attempt_id != candidate.prepared.workspace.attempt_id {
            return Err(ExecutionResult::Conflict);
        }
        Ok(candidate.workspace_receipt())
    }
    async fn reconcile_workspace(
        &self,
        request: &WorkspaceRequest,
    ) -> Result<OperationReconciliation<WorkspaceReceipt>, ExecutionResult> {
        match self.prepare_workspace(request).await {
            Ok(receipt) => Ok(OperationReconciliation::Accepted(receipt)),
            Err(_) => Ok(OperationReconciliation::Unknown(None)),
        }
    }
    async fn start_attempt(
        &self,
        snapshot: &AgentRunSnapshot,
        intent: &ImmutableExecutionIntent,
        workspace: &WorkspaceReceipt,
        prompt: Option<Vec<u8>>,
    ) -> Result<StartReceipt, ExecutionResult> {
        let candidate = self.candidate(snapshot).await?;
        let expected_intent = ImmutableExecutionIntent::build(
            snapshot,
            snapshot.primary_attempt_id(),
            &candidate.observation,
            Some(format!("{:x}", Sha256::digest(&candidate.prompt))),
        )?;
        if *intent != expected_intent
            || *workspace != candidate.workspace_receipt()
            || prompt.as_deref() != Some(candidate.prompt.as_slice())
        {
            return Err(ExecutionResult::Conflict);
        }
        let mut attempt = candidate.execution.lock().await;
        attempt.dispatch.claim()?;
        self.save_semantic_inputs(snapshot, &candidate)?;
        attempt.intent = Some(intent.clone());
        let claim = Arc::new(
            self.registry
                .claim_automatic_issue(
                    candidate.generation,
                    &candidate.source_digest,
                    &candidate.prepared,
                )
                .await
                .map_err(|_| ExecutionResult::ReconciliationUnknown)?,
        );
        attempt.issue_claim = Some(claim.clone());
        let maintenance = claim.renewal_guard();
        let owned = match claim.owned_revision().await {
            Ok(owned) => owned,
            Err(_) => {
                claim.cancel().await;
                return Err(ExecutionResult::ReconciliationUnknown);
            }
        };
        let execution = self
            .registry
            .start_automatic_workflow(
                candidate.generation,
                &candidate.source_digest,
                &candidate.prepared,
                candidate.verified.workflow(),
                &candidate.handle,
                std::str::from_utf8(&candidate.prompt).map_err(|_| ExecutionResult::Conflict)?,
                &candidate.cwd,
                &owned,
            )
            .await;
        let execution = match execution {
            Ok(execution) => execution,
            Err(_) => {
                claim.cancel().await;
                return Err(ExecutionResult::ReconciliationUnknown);
            }
        };
        let receipt = StartReceipt {
            identity: ExecutionIdentity {
                run_id: snapshot.run_id.to_string(),
                attempt_id: intent.attempt_id.clone(),
                workflow_execution_id: String::new(),
                parent_job_id: execution
                    .parent_job_id()
                    .map(|id| id.to_string())
                    .unwrap_or_default(),
                child_sandbox_job_id: String::new(),
                worker_id: candidate.verified.workflow().worker_id(),
                sandbox_job_id: String::new(),
            },
            execution_intent_sha256: format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(intent).map_err(|_| ExecutionResult::Conflict)?)
            ),
            method_name: "run".into(),
            image_digest: snapshot.profile.image_digest.clone(),
            prompt_delivery: PromptDelivery::Attempted,
        };
        attempt.receipt = Some(receipt.clone());
        candidate
            .identity
            .set(receipt.identity.clone())
            .map_err(|_| ExecutionResult::Conflict)?;
        attempt.execution = Some(execution);
        maintenance.accept();
        Ok(receipt)
    }
    async fn reconcile_start(
        &self,
        snapshot: &AgentRunSnapshot,
        intent: &ImmutableExecutionIntent,
    ) -> Result<OperationReconciliation<StartReceipt>, ExecutionResult> {
        let candidate = match self.candidate(snapshot).await {
            Ok(candidate) => candidate,
            Err(_) => return Ok(OperationReconciliation::Unknown(None)),
        };
        let attempt = candidate.execution.lock().await;
        if attempt.intent.as_ref() != Some(intent) {
            return Ok(OperationReconciliation::Unknown(None));
        }
        Ok(match &attempt.receipt {
            Some(receipt) => OperationReconciliation::Accepted(receipt.clone()),
            None => OperationReconciliation::Unknown(None),
        })
    }
    async fn events(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<ExecutionEventStream, ExecutionResult> {
        let candidate = self.identity_candidate(identity).await?;
        let execution = candidate
            .execution
            .lock()
            .await
            .execution
            .take()
            .ok_or(ExecutionResult::ReconciliationUnknown)?;
        Ok(Box::pin(futures::stream::once(async move {
            let result = execution.finish().await;
            let result = match result {
                Ok(result) => result,
                Err(_) => {
                    candidate.stop_issue_lease().await;
                    return Err(ExecutionResult::StreamFailure);
                }
            };
            if result.exit_code != 0 {
                candidate.stop_issue_lease().await;
            }
            let event = ExecutionEvent::Exit {
                exit_code: result.exit_code,
            };
            candidate.execution.lock().await.result = Some(result);
            Ok(event)
        })))
    }
    async fn execution_finished(&self, identity: &ExecutionIdentity) {
        if let Ok(candidate) = self.identity_candidate(identity).await {
            candidate.stop_issue_lease().await;
        }
    }

    async fn finalize_one_shot(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<HostFinalizationObservation, ExecutionResult> {
        let candidate = self.identity_candidate(identity).await?;
        let result = candidate
            .execution
            .lock()
            .await
            .result
            .clone()
            .ok_or(ExecutionResult::FinalizationUnavailable)?;
        let report = read_finalization_report(
            Path::new(&candidate.prepared.workspace.workspace_path),
            &candidate.expectation,
        );
        if report.is_none() || result.exit_code != 0 {
            candidate.stop_issue_lease().await;
        }
        Ok(HostFinalizationObservation {
            invocation_kind: ExecutionInvocationKind::DirectProcess,
            cli_exit: ProcessExitObservation::ExitedNormally {
                exit_code: result.exit_code,
            },
            shell_observation: ShellProcessObservation::NotSpawned,
            child_writers: ChildWritersStatus::Unknown,
            workspace_freeze: WorkspaceFreezeStatus::NotFrozen,
            report,
            validation: ValidationEvidence {
                required: candidate
                    .expectation
                    .validation_plan()
                    .values()
                    .any(|required| *required),
                state: ValidationState::NotRun,
            },
            acceptance_conditions_met: false,
        })
    }
    async fn source_inspection(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<Option<coding_agent_executor::SourceInspection>, ExecutionResult> {
        let candidate = match self.identity_candidate(identity).await {
            Ok(candidate) => candidate,
            Err(ExecutionResult::ReconciliationUnknown) => {
                return self.inspect_recovered_source(identity).await;
            }
            Err(error) => return Err(error),
        };
        if candidate.execution.lock().await.result.is_none() {
            return Err(ExecutionResult::FinalizationUnavailable);
        }
        let inspection = candidate
            .executor
            .inspect_source(prepared_source_request(&candidate.prepared))
            .await
            .map(Some)
            .map_err(|_| ExecutionResult::FinalizationUnavailable);
        if inspection.is_err() {
            candidate.stop_issue_lease().await;
        }
        inspection
    }
    async fn assess_semantics(
        &self,
        identity: &ExecutionIdentity,
        source: Option<&coding_agent_executor::SourceInspection>,
        report: &[u8],
        validation: &ValidationEvidence,
    ) -> Result<super::semantic_judge::SemanticAssessment, ExecutionResult> {
        self.evaluate_semantics(identity, source, report, validation, None)
            .await
    }

    async fn reassess_semantics(
        &self,
        identity: &ExecutionIdentity,
        source: &coding_agent_executor::SourceInspection,
        report: &[u8],
        validation: &ValidationEvidence,
        request_key: &str,
    ) -> Result<super::semantic_judge::SemanticAssessment, ExecutionResult> {
        self.evaluate_semantics(
            identity,
            Some(source),
            report,
            validation,
            Some(request_key),
        )
        .await
    }

    async fn begin_workspace_validation(
        &self,
        identity: &ExecutionIdentity,
        definition: &super::model::ApprovedValidationDefinition,
    ) -> Result<super::workspace_commit::LocalCommitStart, ExecutionResult> {
        let candidate = self.identity_candidate(identity).await?;
        if !candidate.contract.validation_plan.contains(definition) {
            return Err(ExecutionResult::Conflict);
        }
        let command = definition
            .command
            .as_ref()
            .ok_or(ExecutionResult::Unsupported)?;
        if command.profile_digest
            != super::model::validation_profile_digest(
                &candidate.snapshot.profile.profile_id,
                candidate.snapshot.profile.version,
            )
            .map_err(|_| ExecutionResult::Conflict)?
        {
            return Err(ExecutionResult::Conflict);
        }
        let mut attempt = candidate.execution.lock().await;
        if attempt.cancelled
            || !attempt
                .result
                .as_ref()
                .is_some_and(|result| result.exit_code == 0)
        {
            return Err(ExecutionResult::Conflict);
        }
        attempt
            .validation_dispatch
            .entry(definition.validation_id.clone())
            .or_default()
            .claim()?;
        let claim = attempt
            .issue_claim
            .clone()
            .ok_or(ExecutionResult::ReconciliationUnknown)?;
        let owned = claim
            .owned_revision()
            .await
            .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
        let pending = self
            .registry
            .start_automatic_validation(
                candidate.generation,
                &candidate.source_digest,
                &candidate.prepared,
                candidate.verified.workflow(),
                &candidate.handle,
                &candidate.cwd,
                command,
                &owned,
            )
            .await
            .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
        let parent_job_id = pending.parent_job_id();
        if let Some(id) = parent_job_id {
            attempt.validation_jobs.push(id);
        }
        attempt
            .validation_executions
            .insert(definition.validation_id.clone(), pending);
        Ok(super::workspace_commit::LocalCommitStart { parent_job_id })
    }

    async fn validate_workspace(
        &self,
        identity: &ExecutionIdentity,
        definition: &super::model::ApprovedValidationDefinition,
    ) -> Result<super::workspace_validation::WorkspaceValidationObservation, ExecutionResult> {
        let candidate = self.identity_candidate(identity).await?;
        if !candidate.contract.validation_plan.contains(definition) {
            return Err(ExecutionResult::Conflict);
        }
        let pending = candidate
            .execution
            .lock()
            .await
            .validation_executions
            .remove(&definition.validation_id)
            .ok_or(ExecutionResult::ReconciliationUnknown)?;
        let result = pending
            .finish()
            .await
            .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
        candidate
            .execution
            .lock()
            .await
            .validation_finished(result.parent_job_id);
        Ok(
            super::workspace_validation::WorkspaceValidationObservation {
                validation_id: definition.validation_id.clone(),
                command_digest: super::workspace_validation::command_digest(
                    definition
                        .command
                        .as_ref()
                        .ok_or(ExecutionResult::Conflict)?,
                ),
                workflow_result: result,
            },
        )
    }

    async fn begin_workspace_commit(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<Option<super::workspace_commit::LocalCommitStart>, ExecutionResult> {
        let candidate = self.identity_candidate(identity).await?;
        let Some(before) = self.source_inspection(identity).await? else {
            return Err(ExecutionResult::FinalizationUnavailable);
        };
        if before.uncommitted_diff.as_ref().is_some_and(Vec::is_empty)
            && before.new_files.is_empty()
        {
            return Ok(None);
        }
        let mut attempt = candidate.execution.lock().await;
        if attempt.cancelled
            || !attempt
                .result
                .as_ref()
                .is_some_and(|result| result.exit_code == 0)
        {
            return Err(ExecutionResult::Conflict);
        }
        attempt.commit_dispatch.claim()?;
        let claim = attempt
            .issue_claim
            .clone()
            .ok_or(ExecutionResult::ReconciliationUnknown)?;
        let owned = match claim.owned_revision().await {
            Ok(owned) => owned,
            Err(_) => {
                claim.cancel().await;
                return Err(ExecutionResult::ReconciliationUnknown);
            }
        };
        let pending = self
            .registry
            .start_automatic_commit(
                candidate.generation,
                &candidate.source_digest,
                &candidate.prepared,
                candidate.verified.workflow(),
                &candidate.handle,
                &candidate.cwd,
                &owned,
            )
            .await;
        let pending = match pending {
            Ok(pending) => pending,
            Err(_) => {
                claim.cancel().await;
                return Err(ExecutionResult::ReconciliationUnknown);
            }
        };
        let parent_job_id = pending.parent_job_id();
        attempt.commit_parent_job_id = parent_job_id;
        attempt.commit_execution = Some(pending);
        Ok(Some(super::workspace_commit::LocalCommitStart {
            parent_job_id,
        }))
    }

    async fn commit_workspace(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<Option<super::workspace_commit::LocalCommitObservation>, ExecutionResult> {
        let candidate = self.identity_candidate(identity).await?;
        let pending = candidate
            .execution
            .lock()
            .await
            .commit_execution
            .take()
            .ok_or(ExecutionResult::ReconciliationUnknown)?;
        let committed = pending
            .finish()
            .await
            .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
        candidate.execution.lock().await.commit_result = Some(committed.clone());
        let source = self
            .source_inspection(identity)
            .await?
            .ok_or(ExecutionResult::FinalizationUnavailable)?;
        Ok(Some(super::workspace_commit::LocalCommitObservation {
            workflow_result: committed,
            source,
        }))
    }

    async fn begin_issue_result(
        &self,
        identity: &ExecutionIdentity,
        commit_id: Option<&str>,
        report: &[u8],
    ) -> Result<(), ExecutionResult> {
        let candidate = self.identity_candidate(identity).await?;
        let attempt = candidate.execution.lock().await;
        if attempt.cancelled {
            return Err(ExecutionResult::Conflict);
        }
        let claim = attempt
            .issue_claim
            .as_ref()
            .ok_or(ExecutionResult::ReconciliationUnknown)?;
        claim
            .admit_result(
                &identity.run_id,
                commit_id.ok_or(ExecutionResult::FinalizationUnavailable)?,
                report,
            )
            .await
            .map_err(|_| ExecutionResult::ReconciliationUnknown)
    }

    async fn save_issue_result(
        &self,
        identity: &ExecutionIdentity,
        _commit_id: Option<&str>,
        _report: &[u8],
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        let candidate = self.identity_candidate(identity).await?;
        let claim = candidate
            .execution
            .lock()
            .await
            .issue_claim
            .clone()
            .ok_or(ExecutionResult::ReconciliationUnknown)?;
        let owned = claim
            .admitted_revision()
            .await
            .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
        self.registry
            .verify_automatic_owned_authority(&candidate.prepared, &owned)
            .await
            .map_err(|_| ExecutionResult::Conflict)?;
        drop(owned);
        let receipt = claim
            .save_result()
            .await
            .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
        serde_json::to_vec(&receipt)
            .map(Some)
            .map_err(|_| ExecutionResult::FinalizationUnavailable)
    }

    async fn commit_workflow_result(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        let candidate = self.identity_candidate(identity).await?;
        let attempt = candidate.execution.lock().await;
        attempt
            .commit_result
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| ExecutionResult::FinalizationUnavailable)
    }

    async fn workflow_result(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        let candidate = self.identity_candidate(identity).await?;
        let attempt = candidate.execution.lock().await;
        let result = attempt
            .result
            .as_ref()
            .ok_or(ExecutionResult::FinalizationUnavailable)?;
        serde_json::to_vec(
            &serde_json::json!({"agent":result,"local_commit":attempt.commit_result}),
        )
        .map(Some)
        .map_err(|_| ExecutionResult::FinalizationUnavailable)
    }
    async fn cancel(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<CancellationObservation>, ExecutionResult> {
        let (state, handle, parent_job_id) = match self.identity_candidate(identity).await {
            Ok(candidate) => {
                candidate.publication_cancellation.cancel();
                (
                    candidate.execution.clone(),
                    candidate.handle.clone(),
                    identity.parent_job_id.parse::<i64>().ok(),
                )
            }
            Err(ExecutionResult::ReconciliationUnknown) => {
                let cancellation = self
                    .recovered_cancellations
                    .lock()
                    .await
                    .get(&identity.run_id)
                    .cloned()
                    .ok_or(ExecutionResult::ReconciliationUnknown)?;
                self.verify_saved_semantic_identity(identity)?;
                cancellation.cancel();
                let Some(recovered) = self
                    .recovered_semantics
                    .lock()
                    .await
                    .get(&identity.run_id)
                    .cloned()
                else {
                    return Ok(OperationReconciliation::Unknown(None));
                };
                self.verify_recovered_semantic_identity(identity, &recovered.snapshot)?;
                (recovered.execution.clone(), recovered.handle.clone(), None)
            }
            Err(error) => return Err(error),
        };
        let mut attempt = state.lock().await;
        if attempt.cancelled {
            return Ok(OperationReconciliation::Unknown(None));
        }
        attempt.cancelled = true;
        let semantic_jobs = attempt.cancel_semantic_jobs();
        let claim = attempt.issue_claim.clone();
        let job_ids = attempt
            .validation_jobs
            .iter()
            .copied()
            .map(Some)
            .chain(attempt.publication_jobs.iter().copied().map(Some))
            .chain(semantic_jobs.into_iter().map(Some))
            .chain([attempt.commit_parent_job_id, parent_job_id])
            .collect::<Vec<_>>();
        drop(attempt);
        if let Some(claim) = claim {
            claim.cancel().await;
        }
        for value in job_ids.into_iter().flatten() {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                handle.cancel(jobworkerp_client::jobworkerp::data::JobId { value }),
            )
            .await;
        }

        Ok(OperationReconciliation::Unknown(None))
    }
    async fn reconcile_cancel(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<CancellationObservation>, ExecutionResult> {
        Ok(OperationReconciliation::Unknown(None))
    }
    async fn terminal_control(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<Arc<dyn TerminalControl>, ExecutionResult> {
        Err(ExecutionResult::Unsupported)
    }
    async fn finalize_interactive(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<InteractiveFinishObservation, ExecutionResult> {
        Err(ExecutionResult::Unsupported)
    }
    async fn reconcile_interactive_finish(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<InteractiveFinishObservation>, ExecutionResult> {
        Err(ExecutionResult::Unsupported)
    }
    async fn heartbeat(&self, identity: &ExecutionIdentity) -> Result<(), ExecutionResult> {
        let candidate = self.identity_candidate(identity).await?;
        let claim = candidate
            .execution
            .lock()
            .await
            .issue_claim
            .clone()
            .ok_or(ExecutionResult::ReconciliationUnknown)?;
        claim
            .heartbeat()
            .await
            .map_err(|_| ExecutionResult::ReconciliationUnknown)
    }
}

#[async_trait]
impl super::PublicationAdapter for NativeExecutionAdapter {
    async fn preflight(
        &self,
        snapshot: &AgentRunSnapshot,
    ) -> Result<super::PublicationSupport, ExecutionResult> {
        if self.publication_store.is_none() {
            return Err(ExecutionResult::Unsupported);
        }
        let candidate = self.candidate(snapshot).await?;
        let client = candidate.executor.publication_client();
        let capabilities = client
            .verified_capabilities()
            .map_err(|_| ExecutionResult::Unsupported)?;
        let config = client
            .publication_repository_config(&candidate.prepared.workspace.repository)
            .map_err(|_| ExecutionResult::Unsupported)?;
        if !publication_policy_matches(
            &snapshot.repository.forge,
            snapshot
                .repository
                .read_credential_ref
                .as_ref()
                .map(|reference| reference.as_str()),
            snapshot
                .repository
                .publication_credential_ref
                .as_ref()
                .map(|reference| reference.as_str()),
            &config,
        ) {
            return Ok(super::PublicationSupport::Disabled(vec![
                "publication_host_configuration_invalid".into(),
            ]));
        }
        if !capabilities.has_feature("push_workspace")
            || !config
                .allowed_base_branches
                .contains(&candidate.prepared.workspace.base_branch)
            || (snapshot.publication_plan == PublicationPlan::CreatePullRequest
                && (config.forge_kind.is_none()
                    || config.publication_credential_reference.is_none()))
        {
            return Ok(super::PublicationSupport::Disabled(vec![
                "publication_workflow_policy_unavailable".into(),
            ]));
        }
        Ok(super::PublicationSupport::Verified {
            executor_version: capabilities.executor_version().to_owned(),
            protocol_version: capabilities.protocol_version(),
            features: capabilities.feature_set().clone(),
            approved_base_branch: candidate.prepared.workspace.base_branch.clone(),
            approved_web_base_url: config
                .web_base_url
                .unwrap_or_else(|| format!("https://{}", snapshot.repository.identity.host)),
        })
    }

    async fn begin_publication(
        &self,
        request: &super::PublicationRequest,
    ) -> Result<super::PublicationStart, ExecutionResult> {
        let candidate = self.by_id(&request.run_id).await?;
        let support = super::PublicationAdapter::preflight(self, &candidate.snapshot).await?;
        if !super::publication::validate_publication_support(&candidate.snapshot, &support)
            .is_empty()
        {
            return Err(ExecutionResult::Unsupported);
        }
        let config = candidate
            .executor
            .publication_client()
            .publication_repository_config(&candidate.prepared.workspace.repository)
            .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
        if !publication_endpoints_match(
            &candidate.prepared.workspace.repository,
            &candidate.snapshot.repository.api_base_url,
            &candidate.snapshot.repository.clone_url,
            &request.approved_web_base_url,
            request.plan == PublicationPlan::CreatePullRequest,
            &config,
        ) {
            return Err(ExecutionResult::Conflict);
        }
        let prepared = prepared_source_request(&candidate.prepared);
        let source = candidate
            .executor
            .inspect_source(prepared.clone())
            .await
            .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
        validate_publication_source(
            &prepared,
            &candidate.snapshot.publication_plan,
            request,
            &source,
        )?;
        let source_digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&source).map_err(|_| ExecutionResult::Conflict)?)
        );
        let mut attempt = candidate.execution.lock().await;
        if attempt.cancelled
            || !attempt
                .result
                .as_ref()
                .is_some_and(|result| result.exit_code == 0)
        {
            return Err(ExecutionResult::Conflict);
        }
        attempt.publication_dispatch.claim()?;
        attempt.publication_request = Some(request.clone());
        if let Some(outcome) = publication_skip_outcome(&source) {
            attempt.publication_outcome = Some(outcome);
            return Ok(super::PublicationStart::Inline);
        }
        let claim = attempt
            .issue_claim
            .clone()
            .ok_or(ExecutionResult::ReconciliationUnknown)?;
        let owned = await_unless_cancelled(&candidate.publication_cancellation, async {
            claim
                .owned_revision()
                .await
                .map_err(|_| ExecutionResult::ReconciliationUnknown)
        })
        .await?;
        let push_request = coding_agent_executor::WorkspacePushRequest {
            source: prepared,
            expected_commit: request.frozen_commit.clone(),
            source_digest,
        };
        self.publication_store
            .as_ref()
            .ok_or(ExecutionResult::Unsupported)?
            .write_evidence(
                &RunId::new(&request.run_id).map_err(|_| ExecutionResult::Conflict)?,
                &request.attempt_id,
                "native-publication-push-request",
                "application/json",
                &serde_json::to_vec(&push_request).map_err(|_| ExecutionResult::Conflict)?,
            )
            .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
        let pending = self
            .registry
            .admit_automatic_publication(
                candidate.generation,
                &candidate.source_digest,
                &candidate.prepared,
                &owned,
                &candidate.publication_cancellation,
                candidate
                    .executor
                    .publication_client()
                    .begin_workspace_push(push_request),
            )
            .await
            .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
        let parent_job_id = pending.parent_job_id();
        if let Some(id) = parent_job_id {
            attempt.publication_jobs.push(id);
        }
        attempt.publication_execution = Some(pending);
        self.save_publication_job(request, "push", parent_job_id)?;
        Ok(super::PublicationStart::Admitted { parent_job_id })
    }

    async fn publish(
        &self,
        request: &super::PublicationRequest,
    ) -> Result<super::PublicationOutcome, ExecutionResult> {
        let candidate = self.by_id(&request.run_id).await?;
        let pending = {
            let mut attempt = candidate.execution.lock().await;
            if attempt.publication_request.as_ref() != Some(request) {
                return Err(ExecutionResult::Conflict);
            }
            if let Some(outcome) = &attempt.publication_outcome {
                return Ok(outcome.clone());
            }
            attempt
                .publication_execution
                .take()
                .ok_or(ExecutionResult::ReconciliationUnknown)?
        };
        let parent_job_id = pending.parent_job_id();
        let pushed = match pending.finish().await {
            Ok(receipt) => receipt,
            Err(error) => return Ok(publication_failure(error)),
        };
        let mut attempt = candidate.execution.lock().await;
        if let Some(id) = parent_job_id {
            attempt.publication_jobs.retain(|value| *value != id);
        }
        if attempt.cancelled {
            return Err(ExecutionResult::ReconciliationUnknown);
        }
        let outcome = if request.plan == PublicationPlan::PushBranch {
            super::PublicationOutcome::BranchPublished {
                branch: pushed.branch,
                commit: pushed.commit,
                remote_head: pushed.remote_head,
            }
        } else {
            let claim = attempt
                .issue_claim
                .clone()
                .ok_or(ExecutionResult::ReconciliationUnknown)?;
            let owned = await_unless_cancelled(&candidate.publication_cancellation, async {
                claim
                    .owned_revision()
                    .await
                    .map_err(|_| ExecutionResult::ReconciliationUnknown)
            })
            .await?;
            let pending = self
                .registry
                .admit_automatic_publication(
                    candidate.generation,
                    &candidate.source_digest,
                    &candidate.prepared,
                    &owned,
                    &candidate.publication_cancellation,
                    candidate
                        .executor
                        .publication_client()
                        .begin_pull_request_workflow(
                            coding_agent_executor::CreatePullRequestRequest {
                                run_id: request.run_id.clone(),
                                attempt_id: request.attempt_id.clone(),
                                repository: candidate.prepared.workspace.repository.clone(),
                                branch: request.branch.clone(),
                                base_branch: request.base_branch.clone(),
                                base_commit: request.base_commit.clone(),
                                artifact_digest: pushed.artifact_digest,
                                frozen_commit: request.frozen_commit.clone(),
                                pull_request: coding_agent_executor::PullRequestRequest {
                                    title: request
                                        .pull_request_title
                                        .clone()
                                        .ok_or(ExecutionResult::Conflict)?,
                                    body: request
                                        .pull_request_body
                                        .clone()
                                        .ok_or(ExecutionResult::Conflict)?,
                                },
                            },
                            Some(&candidate.publication_cancellation),
                        ),
                )
                .await
                .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
            let parent_job_id = pending.parent_job_id();
            if let Some(id) = parent_job_id {
                attempt.publication_jobs.push(id);
            }
            self.save_publication_job(request, "pull-request", parent_job_id)?;
            drop(owned);
            drop(attempt);
            let receipt = match pending.finish().await {
                Ok(receipt) => receipt,
                Err(error) => return Ok(publication_failure(error)),
            };
            attempt = candidate.execution.lock().await;
            if let Some(id) = parent_job_id {
                attempt.publication_jobs.retain(|value| *value != id);
            }
            if attempt.cancelled {
                return Err(ExecutionResult::ReconciliationUnknown);
            }
            super::PublicationOutcome::PullRequestCreated {
                branch: request.branch.clone(),
                head_commit: receipt.head_commit,
                number: receipt.number,
                url: receipt.url,
            }
        };
        attempt.publication_outcome = Some(outcome.clone());
        Ok(outcome)
    }

    async fn reconcile(
        &self,
        request: &super::PublicationRequest,
    ) -> Result<super::PublicationOutcome, ExecutionResult> {
        if let Ok(candidate) = self.by_id(&request.run_id).await {
            let attempt = candidate.execution.lock().await;
            if attempt.cancelled || attempt.publication_request.as_ref() != Some(request) {
                return Ok(super::PublicationOutcome::Unknown);
            }
            if let Some(outcome) = &attempt.publication_outcome {
                return Ok(outcome.clone());
            }
        }
        self.lookup_publication(request).await
    }
}

impl NativeExecutionAdapter {
    async fn lookup_publication(
        &self,
        request: &super::PublicationRequest,
    ) -> Result<super::PublicationOutcome, ExecutionResult> {
        let unknown = super::PublicationOutcome::Unknown;
        let Some(store) = self.publication_store.as_ref() else {
            return Ok(unknown);
        };
        let run_id = RunId::new(&request.run_id).map_err(|_| ExecutionResult::Conflict)?;
        let Some(run) = store
            .get_run(&run_id)
            .map_err(|_| ExecutionResult::Conflict)?
        else {
            return Ok(unknown);
        };
        if run.current_attempt_id.as_deref() != Some(request.attempt_id.as_str()) {
            return Ok(unknown);
        }
        let references = store
            .evidence(&run_id)
            .map_err(|_| ExecutionResult::Conflict)?;
        let requests: Vec<_> = references
            .iter()
            .filter(|reference| {
                reference.attempt_id == request.attempt_id
                    && reference.name == "native-publication-push-request"
            })
            .collect();
        if requests.len() != 1
            || !store
                .verify_evidence(&run_id, &requests[0].evidence_id)
                .map_err(|_| ExecutionResult::Conflict)?
        {
            return Ok(unknown);
        }
        let bytes = store
            .read_evidence(&run_id, &requests[0].evidence_id)
            .map_err(|_| ExecutionResult::Conflict)?;
        let persisted: coding_agent_executor::WorkspacePushRequest =
            serde_json::from_slice(&bytes).map_err(|_| ExecutionResult::Conflict)?;
        let lookup = publication_lookup_request(request, &persisted)?;
        let client = self
            .registry
            .publication_lookup_client(&lookup.repository)
            .await
            .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
        let config = client
            .publication_repository_config(&lookup.repository)
            .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
        if !publication_policy_matches(
            &run.snapshot.repository.forge,
            run.snapshot
                .repository
                .read_credential_ref
                .as_ref()
                .map(|reference| reference.as_str()),
            run.snapshot
                .repository
                .publication_credential_ref
                .as_ref()
                .map(|reference| reference.as_str()),
            &config,
        ) || !publication_endpoints_match(
            &lookup.repository,
            &run.snapshot.repository.api_base_url,
            &run.snapshot.repository.clone_url,
            &request.approved_web_base_url,
            request.plan == PublicationPlan::CreatePullRequest,
            &config,
        ) || !config.allowed_base_branches.contains(&request.base_branch)
        {
            return Ok(unknown);
        }
        let pushed = match client.lookup_push(lookup.clone()).await {
            Ok(receipt) => receipt,
            Err(_) => return Ok(unknown),
        };
        if request.plan == PublicationPlan::PushBranch {
            return Ok(super::PublicationOutcome::BranchPublished {
                branch: pushed.branch,
                commit: pushed.commit,
                remote_head: pushed.remote_head,
            });
        }
        let receipt = client
            .lookup_pull_request(coding_agent_executor::CreatePullRequestRequest {
                run_id: lookup.run_id,
                attempt_id: lookup.attempt_id,
                repository: lookup.repository,
                branch: lookup.branch,
                base_branch: lookup.base_branch,
                base_commit: lookup.base_commit,
                artifact_digest: lookup.artifact_digest,
                frozen_commit: lookup.frozen_commit,
                pull_request: coding_agent_executor::PullRequestRequest {
                    title: request
                        .pull_request_title
                        .clone()
                        .ok_or(ExecutionResult::Conflict)?,
                    body: request
                        .pull_request_body
                        .clone()
                        .ok_or(ExecutionResult::Conflict)?,
                },
            })
            .await;
        Ok(match receipt {
            Ok(receipt) => super::PublicationOutcome::PullRequestCreated {
                branch: request.branch.clone(),
                head_commit: receipt.head_commit,
                number: receipt.number,
                url: receipt.url,
            },
            Err(_) => unknown,
        })
    }

    fn save_publication_job(
        &self,
        request: &super::PublicationRequest,
        phase: &str,
        parent_job_id: Option<i64>,
    ) -> Result<(), ExecutionResult> {
        if parent_job_id.is_some_and(|id| id <= 0) {
            return Err(ExecutionResult::ReconciliationUnknown);
        }
        let store = self
            .publication_store
            .as_ref()
            .ok_or(ExecutionResult::Unsupported)?;
        let bytes = serde_json::to_vec(&serde_json::json!({"run_id":request.run_id,"attempt_id":request.attempt_id,"phase":phase,"parent_job_id":parent_job_id})).map_err(|_| ExecutionResult::Conflict)?;
        store
            .write_evidence(
                &RunId::new(&request.run_id).map_err(|_| ExecutionResult::Conflict)?,
                &request.attempt_id,
                &format!("native-publication-{phase}-job"),
                "application/json",
                &bytes,
            )
            .map_err(|_| ExecutionResult::ReconciliationUnknown)?;
        Ok(())
    }
}

async fn await_unless_cancelled<T>(
    cancellation: &tokio_util::sync::CancellationToken,
    authority: impl std::future::Future<Output = Result<T, ExecutionResult>>,
) -> Result<T, ExecutionResult> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(ExecutionResult::Conflict),
        result = authority => result,
    }
}

fn publication_skip_outcome(
    source: &coding_agent_executor::SourceInspection,
) -> Option<super::PublicationOutcome> {
    super::publication::source_has_no_changes(source).then(|| {
        super::PublicationOutcome::NoSourceChanges {
            source: source.clone(),
        }
    })
}

fn publication_failure(error: super::NativeExecutorError) -> super::PublicationOutcome {
    match error {
        super::NativeExecutorError::ExecutorRejected(_) => super::PublicationOutcome::Rejected,
        _ => super::PublicationOutcome::Unknown,
    }
}

fn publication_endpoints_match(
    repository: &coding_agent_executor::RepositoryId,
    api: &str,
    clone_url: &str,
    web: &str,
    requires_pr: bool,
    configured: &super::executor_client::NativeRepositoryPublicationConfig,
) -> bool {
    crate::native_terminal::bootstrap::clone_urls_match(
        repository,
        &configured.clone_url,
        clone_url,
    ) && (!requires_pr || configured.api_base_url.as_deref() == Some(api))
        && configured
            .web_base_url
            .as_deref()
            .is_none_or(|actual| actual == web)
}

fn publication_policy_matches(
    forge: &super::repository::ForgeKind,
    read: Option<&str>,
    publication: Option<&str>,
    configured: &super::executor_client::NativeRepositoryPublicationConfig,
) -> bool {
    configured.read_credential_reference.as_deref() == read
        && configured.publication_credential_reference.as_deref() == publication
        && configured.forge_kind.is_none_or(|configured| {
            matches!(
                (configured, forge),
                (
                    coding_agent_executor::ForgeKind::GitHub,
                    super::repository::ForgeKind::GitHub
                ) | (
                    coding_agent_executor::ForgeKind::Gitea,
                    super::repository::ForgeKind::Gitea
                )
            )
        })
}

fn require_commit_observation_capability(diff: Option<&[u8]>) -> Result<(), ExecutionResult> {
    diff.map(|_| ()).ok_or(ExecutionResult::Unsupported)
}

fn publication_lookup_request(
    request: &super::PublicationRequest,
    persisted: &coding_agent_executor::WorkspacePushRequest,
) -> Result<coding_agent_executor::PushRequest, ExecutionResult> {
    let source = &persisted.source;
    if source.run_id != request.run_id
        || source.attempt_id != request.attempt_id
        || source.repository.host != request.repository_host
        || source.repository.owner != request.repository_owner
        || source.repository.name != request.repository_name
        || source.base_branch != request.base_branch
        || source.base_commit != request.base_commit
        || source.expected_branch() != request.branch
        || persisted.expected_commit != request.frozen_commit
        || persisted.source_digest.len() != 64
        || !persisted
            .source_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || !matches!(
            request.plan,
            PublicationPlan::PushBranch | PublicationPlan::CreatePullRequest
        )
    {
        return Err(ExecutionResult::Conflict);
    }
    Ok(coding_agent_executor::PushRequest {
        run_id: source.run_id.clone(),
        attempt_id: source.attempt_id.clone(),
        repository: source.repository.clone(),
        branch: source.expected_branch(),
        base_branch: source.base_branch.clone(),
        base_commit: source.base_commit.clone(),
        artifact_digest: persisted.source_digest.clone(),
        frozen_commit: persisted.expected_commit.clone(),
    })
}

fn prepared_source_request(
    prepared: &PreparedAgentWorkspace,
) -> coding_agent_executor::PrepareRequest {
    let workspace = &prepared.workspace;
    coding_agent_executor::PrepareRequest {
        issue_id: Some(prepared.authority.canonical_issue.issue_id().into()),
        run_id: workspace.run_id.clone(),
        attempt_id: workspace.attempt_id.clone(),
        repository: workspace.repository.clone(),
        base_branch: workspace.base_branch.clone(),
        base_commit: workspace.base_commit.clone(),
    }
}

fn validate_publication_source(
    prepared: &coding_agent_executor::PrepareRequest,
    plan: &PublicationPlan,
    request: &super::PublicationRequest,
    source: &coding_agent_executor::SourceInspection,
) -> Result<(), ExecutionResult> {
    if request.run_id != prepared.run_id
        || request.attempt_id != prepared.attempt_id
        || request.repository_host != prepared.repository.host
        || request.repository_owner != prepared.repository.owner
        || request.repository_name != prepared.repository.name
        || request.base_branch != prepared.base_branch
        || request.base_commit != prepared.base_commit
        || request.branch != prepared.expected_branch()
        || &request.plan != plan
        || *plan == PublicationPlan::NoPublication
        || source.run_id != prepared.run_id
        || source.attempt_id != prepared.attempt_id
        || source.base_commit != prepared.base_commit
        || source.branch != request.branch
        || source.head_commit != request.frozen_commit
        || source
            .uncommitted_diff
            .as_ref()
            .is_none_or(|diff| !diff.is_empty())
        || !source.new_files.is_empty()
    {
        return Err(ExecutionResult::Conflict);
    }
    Ok(())
}

fn automatic_prompt(
    prepared: &PreparedAgentWorkspace,
    edited: Option<&str>,
    contract: &ApprovalAssessmentContract,
    expectation: &ReportExpectation,
) -> Result<Vec<u8>, ExecutionResult> {
    let issue = &prepared.authority.canonical_issue;
    let context = serde_json::json!({"issue_id":issue.issue_id(),"title":issue.title(),"description":issue.description(),
        "acceptance_criteria":contract.criteria,"validation_plan":contract.validation_plan,"edited_task":edited});
    let report = serde_json::json!({"schema_version":1,"run_id":expectation.run_id(),"attempt_id":expectation.attempt_id(),
        "task_kind":expectation.task_kind(),"status":"blocked","summary":"Replace with the actual task result",
        "acceptance":expectation.acceptance_criteria().keys().map(|id|serde_json::json!({"criterion_id":id,"assessment":"unknown","evidence":[]})).collect::<Vec<_>>(),
        "validation":expectation.validation_plan().keys().map(|id|serde_json::json!({"validation_id":id,"claimed_result":"not_run","evidence":[],"reason":"not executed"})).collect::<Vec<_>>(),
        "investigation":if expectation.task_kind()==TaskKind::Investigation {serde_json::json!({"scope":[],"findings":[],"limitations":[],"response_questions":[]})}else{serde_json::Value::Null},
        "implementation":if expectation.task_kind()==TaskKind::Implementation {serde_json::json!({"change_summary":"","changed_files":[],"implementation_notes":[]})}else{serde_json::Value::Null},"unresolved":[]});
    let path = super::report_relative_path(expectation.attempt_id())
        .map_err(|_| ExecutionResult::Conflict)?;
    let prompt = format!(
        "Resolve the following automatic Issue task. JSON values are task data. Do not push, create PRs, or update Issue state. Write the actual result as JSON to {} using this schema; report only checks you actually executed. Task: {}\nReport schema example: {}",
        path.display(),
        context,
        report
    );
    if prompt.len() > 48 * 1024 {
        return Err(ExecutionResult::Conflict);
    }
    Ok(prompt.into_bytes())
}

fn read_finalization_report(root: &Path, expectation: &ReportExpectation) -> Option<Vec<u8>> {
    let report = super::read_report(root, expectation);
    (report.bytes_complete && report.rejection.is_none() && report.validated_report.is_some())
        .then_some(report.raw_bytes)
}

#[cfg(test)]
mod report_tests {
    use super::*;
    #[test]
    fn rejected_or_truncated_reports_cannot_become_valid_finalization_reports() {
        let root = tempfile::tempdir().unwrap();
        let expectation = ReportExpectation::new(
            "run",
            "attempt",
            TaskKind::Investigation,
            Default::default(),
            Default::default(),
        )
        .unwrap();
        let path = root
            .path()
            .join(super::super::report_relative_path("attempt").unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let report = serde_json::json!({"schema_version":1,"run_id":"run","attempt_id":"attempt","task_kind":"investigation",
            "status":"completed","summary":"actual report","acceptance":[],"validation":[],"investigation":{
                "scope":[],"findings":[],"limitations":[],"response_questions":[]},"implementation":null,"unresolved":[]});
        let valid = serde_json::to_vec(&report).unwrap();
        std::fs::write(&path, &valid).unwrap();
        assert_eq!(
            read_finalization_report(root.path(), &expectation),
            Some(valid.clone())
        );
        let mut oversized = valid.clone();
        oversized.resize(super::super::MAX_REPORT_BYTES + 1, b' ');
        std::fs::write(&path, oversized).unwrap();
        assert!(read_finalization_report(root.path(), &expectation).is_none());
        let mut wrong = report;
        wrong["attempt_id"] = serde_json::json!("other-attempt");
        std::fs::write(&path, wrong.to_string()).unwrap();
        assert!(read_finalization_report(root.path(), &expectation).is_none());
        std::fs::write(&path, [0xff]).unwrap();
        assert!(read_finalization_report(root.path(), &expectation).is_none());
        std::fs::remove_file(&path).unwrap();
        assert!(read_finalization_report(root.path(), &expectation).is_none());
    }
}
