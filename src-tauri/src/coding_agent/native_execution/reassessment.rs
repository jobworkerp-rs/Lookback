use super::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const INPUT_NAME: &str = "native-semantic-inputs";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SavedSemanticInputs {
    snapshot_sha256: String,
    source_request: coding_agent_executor::PrepareRequest,
    issue_title: String,
    issue_description: String,
    settings: crate::commands::llm_settings::LlmSettings,
    #[serde(default)]
    policy: super::super::semantic_judge::SemanticJudgePolicy,
}

pub(super) struct SemanticTarget {
    pub snapshot: AgentRunSnapshot,
    pub contract: ApprovalAssessmentContract,
    pub semantic: SemanticCandidate,
    pub issue_title: String,
    pub issue_description: String,
    pub handle: JobworkerpHandle,
    pub expectation: ReportExpectation,
    pub execution: Arc<Mutex<Attempt>>,
    pub cancellation: tokio_util::sync::CancellationToken,
}

fn snapshot_digest(snapshot: &AgentRunSnapshot) -> Result<String, ExecutionResult> {
    let mut input = snapshot.clone();
    input.approval = None;
    input.auto_done = false;
    prepared_snapshot_digest(INPUT_NAME, &input)
}

pub(super) fn verify_saved_semantic_source(
    source: &coding_agent_executor::PrepareRequest,
    workspace: &WorkspaceReceipt,
    issue_id: &str,
) -> Result<(), ExecutionResult> {
    if source.issue_id.as_deref() != Some(issue_id)
        || source.run_id != workspace.run_id
        || source.attempt_id != workspace.attempt_id
        || source.repository.host != workspace.repository_host
        || source.repository.owner != workspace.repository_owner
        || source.repository.name != workspace.repository_name
        || source.base_commit != workspace.base_commit
        || source.expected_branch() != workspace.branch
        || source.base_branch.is_empty()
    {
        return Err(ExecutionResult::Conflict);
    }
    Ok(())
}

impl NativeExecutionAdapter {
    pub(super) fn save_semantic_inputs(
        &self,
        snapshot: &AgentRunSnapshot,
        candidate: &Candidate,
    ) -> Result<(), ExecutionResult> {
        let (Some(store), Some(semantic)) = (&self.publication_store, &candidate.semantic) else {
            return Ok(());
        };
        let issue = &candidate.prepared.authority.canonical_issue;
        let inputs = SavedSemanticInputs {
            snapshot_sha256: snapshot_digest(snapshot)?,
            source_request: prepared_source_request(&candidate.prepared),
            issue_title: issue.title().into(),
            issue_description: issue.description().into(),
            settings: semantic.settings.clone(),
            policy: semantic.judge.policy(),
        };
        store
            .write_evidence(
                &snapshot.run_id,
                snapshot.primary_attempt_id(),
                INPUT_NAME,
                "application/json",
                &serde_json::to_vec(&inputs).map_err(|_| ExecutionResult::Conflict)?,
            )
            .map_err(|_| ExecutionResult::FinalizationUnavailable)?;
        Ok(())
    }

    fn stored_semantic_evidence<T: DeserializeOwned>(
        &self,
        snapshot: &AgentRunSnapshot,
        name: &str,
    ) -> Result<T, ExecutionResult> {
        let store = self
            .publication_store
            .as_ref()
            .ok_or(ExecutionResult::Unsupported)?;
        let references = store
            .evidence(&snapshot.run_id)
            .map_err(|_| ExecutionResult::FinalizationUnavailable)?;
        let matches: Vec<_> = references
            .iter()
            .filter(|item| item.attempt_id == snapshot.primary_attempt_id() && item.name == name)
            .collect();
        if matches.len() != 1 || matches[0].media_type != "application/json" {
            return Err(ExecutionResult::Conflict);
        }
        let bytes = store
            .read_evidence(&snapshot.run_id, &matches[0].evidence_id)
            .map_err(|_| ExecutionResult::FinalizationUnavailable)?;
        serde_json::from_slice(&bytes).map_err(|_| ExecutionResult::Conflict)
    }

    pub(super) fn saved_semantic_inputs(
        &self,
        snapshot: &AgentRunSnapshot,
    ) -> Result<SavedSemanticInputs, ExecutionResult> {
        let inputs: SavedSemanticInputs = self.stored_semantic_evidence(snapshot, INPUT_NAME)?;
        let workspace: WorkspaceReceipt =
            self.stored_semantic_evidence(snapshot, "workspace-receipt")?;
        if inputs.snapshot_sha256 != snapshot_digest(snapshot)?
            || workspace.run_id != snapshot.run_id.as_str()
            || workspace.attempt_id != snapshot.primary_attempt_id()
            || workspace.repository_host != snapshot.repository.identity.host
            || workspace.repository_owner != snapshot.repository.identity.organization
            || workspace.repository_name != snapshot.repository.identity.repository
            || workspace.base_commit != snapshot.base_commit
            || workspace.branch != snapshot.workspace_branch
        {
            return Err(ExecutionResult::Conflict);
        }
        verify_saved_semantic_source(&inputs.source_request, &workspace, &snapshot.issue_id)?;
        Ok(inputs)
    }

    pub(super) fn recovered_semantic_run(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<super::super::AgentRunRecord, ExecutionResult> {
        let id = RunId::new(&identity.run_id).map_err(|_| ExecutionResult::Conflict)?;
        let run = self
            .publication_store
            .as_ref()
            .ok_or(ExecutionResult::Unsupported)?
            .get_run(&id)
            .map_err(|_| ExecutionResult::Conflict)?
            .ok_or(ExecutionResult::Conflict)?;
        if run.state != super::super::RunState::AwaitingReview
            || run.snapshot.approval.is_none()
            || !run.snapshot.semantic_judge_enabled
            || run.snapshot.execution_mode != super::super::ExecutionMode::OneShot
            || run.current_attempt_id.as_deref() != Some(identity.attempt_id.as_str())
        {
            return Err(ExecutionResult::Conflict);
        }
        if self
            .publication_store
            .as_ref()
            .ok_or(ExecutionResult::Unsupported)?
            .operation(&id, &format!("cancel:{}", identity.attempt_id))
            .map_err(|_| ExecutionResult::Conflict)?
            .is_some()
        {
            return Err(ExecutionResult::Conflict);
        }
        self.verify_recovered_semantic_identity(identity, &run.snapshot)?;
        Ok(run)
    }

    pub(super) fn verify_recovered_semantic_identity(
        &self,
        identity: &ExecutionIdentity,
        snapshot: &AgentRunSnapshot,
    ) -> Result<(), ExecutionResult> {
        let receipt: StartReceipt = self.stored_semantic_evidence(snapshot, "dispatch-receipt")?;
        if receipt.identity != *identity {
            return Err(ExecutionResult::Conflict);
        }
        Ok(())
    }

    pub(super) fn verify_saved_semantic_identity(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<(), ExecutionResult> {
        let id = RunId::new(&identity.run_id).map_err(|_| ExecutionResult::Conflict)?;
        let run = self
            .publication_store
            .as_ref()
            .ok_or(ExecutionResult::Unsupported)?
            .get_run(&id)
            .map_err(|_| ExecutionResult::Conflict)?
            .ok_or(ExecutionResult::Conflict)?;
        if run.current_attempt_id.as_deref() != Some(identity.attempt_id.as_str()) {
            return Err(ExecutionResult::Conflict);
        }
        self.verify_recovered_semantic_identity(identity, &run.snapshot)
    }

    pub(super) async fn semantic_target(
        &self,
        identity: &ExecutionIdentity,
        warm: Option<&Arc<Candidate>>,
    ) -> Result<Arc<SemanticTarget>, ExecutionResult> {
        if let Some(candidate) = warm {
            let semantic = candidate
                .semantic
                .as_ref()
                .ok_or(ExecutionResult::Unsupported)?;
            let issue = &candidate.prepared.authority.canonical_issue;
            return Ok(Arc::new(SemanticTarget {
                snapshot: candidate.snapshot.clone(),
                contract: candidate.contract.clone(),
                semantic: SemanticCandidate {
                    settings: semantic.settings.clone(),
                    judge: semantic.judge.clone(),
                },
                issue_title: issue.title().into(),
                issue_description: issue.description().into(),
                handle: candidate.handle.clone(),
                expectation: candidate.expectation.clone(),
                execution: candidate.execution.clone(),
                cancellation: candidate.publication_cancellation.clone(),
            }));
        }
        let run = self.recovered_semantic_run(identity)?;
        let inputs = self.saved_semantic_inputs(&run.snapshot)?;
        if let Some(target) = self.recovered_semantics.lock().await.get(&identity.run_id) {
            return Ok(target.clone());
        }
        let cancellation = self
            .recovered_cancellations
            .lock()
            .await
            .entry(identity.run_id.clone())
            .or_default()
            .clone();
        let store = self
            .publication_store
            .as_ref()
            .ok_or(ExecutionResult::Unsupported)?;
        let super::super::model::ApprovalAssessmentContractStatus::Versioned { contract, .. } =
            store
                .approval_assessment_contract(&run.snapshot.run_id)
                .map_err(|_| ExecutionResult::Conflict)?
        else {
            return Err(ExecutionResult::Conflict);
        };
        let _gate = await_unless_cancelled(&cancellation, async {
            Ok(self.llm_settings_gate.lock().await)
        })
        .await?;
        if self.registry.llm_settings_snapshot() != inputs.settings
            || self
                .registry
                .semantic_judge_policy()
                .map_err(|_| ExecutionResult::Conflict)?
                != inputs.policy
            || contract.provider.as_ref()
                != Some(&super::super::semantic_judge::provider_selection(
                    &inputs.settings,
                )?)
        {
            return Err(ExecutionResult::Conflict);
        }
        let handle = await_unless_cancelled(&cancellation, async {
            self.registry
                .semantic_judge_handle()
                .await
                .map_err(|_| ExecutionResult::AdapterUnavailable)
        })
        .await?;
        let judge = await_unless_cancelled(
            &cancellation,
            super::super::semantic_judge::RegisteredSemanticJudge::observe_for_settings(
                &handle,
                &inputs.settings,
            ),
        )
        .await?
        .with_policy(inputs.policy)?;
        self.recovered_semantic_run(identity)?;
        if run
            .snapshot
            .workflow
            .semantic_judge_worker_digest
            .as_deref()
            != Some(judge.configuration_digest().as_str())
        {
            return Err(ExecutionResult::Conflict);
        }
        let target = Arc::new(SemanticTarget {
            expectation: ReportExpectation::new(
                identity.run_id.clone(),
                identity.attempt_id.clone(),
                match run.snapshot.purpose {
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
            .map_err(|_| ExecutionResult::Conflict)?,
            snapshot: run.snapshot,
            contract: *contract,
            semantic: SemanticCandidate {
                settings: inputs.settings,
                judge: Arc::new(judge),
            },
            issue_title: inputs.issue_title,
            issue_description: inputs.issue_description,
            handle,
            execution: Arc::new(Mutex::new(Attempt::default())),
            cancellation,
        });
        let mut recovered = self.recovered_semantics.lock().await;
        Ok(recovered
            .entry(identity.run_id.clone())
            .or_insert(target)
            .clone())
    }

    pub(super) async fn inspect_recovered_source(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<Option<coding_agent_executor::SourceInspection>, ExecutionResult> {
        let run = self.recovered_semantic_run(identity)?;
        let inputs = self.saved_semantic_inputs(&run.snapshot)?;
        let client = self
            .registry
            .publication_lookup_client(&inputs.source_request.repository)
            .await
            .map_err(|_| ExecutionResult::AdapterUnavailable)?;
        client
            .inspect_source(inputs.source_request)
            .await
            .map(Some)
            .map_err(|_| ExecutionResult::FinalizationUnavailable)
    }

    pub(super) fn verify_saved_reassessment_issue(
        &self,
        snapshot: &AgentRunSnapshot,
        issue: &crate::commands::issues::IssueListItem,
    ) -> Result<(), ExecutionResult> {
        let inputs = self.saved_semantic_inputs(snapshot)?;
        let super::super::model::ApprovalAssessmentContractStatus::Versioned { contract, .. } =
            self.publication_store
                .as_ref()
                .ok_or(ExecutionResult::Unsupported)?
                .approval_assessment_contract(&snapshot.run_id)
                .map_err(|_| ExecutionResult::Conflict)?
        else {
            return Err(ExecutionResult::Conflict);
        };
        if snapshot.issue_id != issue.id
            || inputs.issue_title != issue.title
            || inputs.issue_description != issue.description
            || contract
                .criteria
                .iter()
                .map(|item| &item.statement)
                .ne(issue.acceptance_criteria_items.iter())
        {
            return Err(ExecutionResult::Conflict);
        }
        Ok(())
    }
}
