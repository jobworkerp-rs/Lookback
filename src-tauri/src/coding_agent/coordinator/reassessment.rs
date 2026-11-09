use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticReassessmentRecord {
    pub run_id: String,
    pub attempt_id: String,
    pub request_key: String,
    pub expected_run_revision: u64,
    pub issue_revision: u64,
    pub source_sha256: String,
    pub source_head_commit: String,
    pub source_is_clean: bool,
    pub report_reference: String,
    pub assessment: crate::coding_agent::semantic_judge::SemanticAssessment,
}

impl CodingAgentCoordinator {
    pub fn read_semantic_reassessment(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        issue_revision: u64,
        request_key: &str,
    ) -> Result<Option<SemanticReassessmentRecord>, CoordinatorError> {
        if !crate::coding_agent::delegation::valid_identifier(request_key, 512) {
            return Err(CoordinatorError::Conflict);
        }
        let run = self.require_run(run_id)?;
        let attempt_id = run
            .current_attempt_id
            .as_deref()
            .ok_or(CoordinatorError::Conflict)?;
        let name = format!("human-semantic-{}", sha256_hex(request_key.as_bytes()));
        let references = self.inner.store.evidence(run_id)?;
        let Some(reference) = references
            .iter()
            .find(|item| item.attempt_id == attempt_id && item.name == name)
        else {
            return Ok(None);
        };
        let bytes = self
            .inner
            .store
            .read_evidence(run_id, &reference.evidence_id)?;
        let record: SemanticReassessmentRecord =
            serde_json::from_slice(&bytes).map_err(|_| CoordinatorError::Conflict)?;
        if record.run_id != run_id.as_str()
            || record.attempt_id != attempt_id
            || record.request_key != request_key
            || record.expected_run_revision != expected_revision
            || record.issue_revision != issue_revision
        {
            return Err(CoordinatorError::Conflict);
        }
        let operation_key = format!("human-semantic:{}", sha256_hex(request_key.as_bytes()));
        let operation = self
            .inner
            .store
            .operation(run_id, &operation_key)?
            .ok_or(CoordinatorError::Conflict)?;
        if operation.attempt_id != attempt_id
            || reference.media_type != JSON_MEDIA_TYPE
            || operation.request
                != (DurableOperationRequest::ReassessSemantics {
                    expected_run_revision: expected_revision,
                    issue_revision,
                    source_sha256: record.source_sha256.clone(),
                    report_reference: record.report_reference.clone(),
                })
        {
            return Err(CoordinatorError::Conflict);
        }
        // A verified result can survive a crash before its journal receipt. Finish
        // that receipt without invoking the provider or changing completion evidence.
        self.inner.store.complete_operation(
            run_id,
            &operation_key,
            super::super::model::DurableOperationResult {
                outcome: DurableOperationOutcome::Succeeded,
                result_digest: Some(reference.sha256.clone()),
                evidence_ids: vec![reference.evidence_id.clone()],
            },
        )?;
        Ok(Some(record))
    }

    /// Record a human-requested judgment without changing the original completion evidence.
    pub async fn reassess_semantics(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        issue_revision: u64,
        request_key: &str,
    ) -> Result<SemanticReassessmentRecord, CoordinatorError> {
        if !crate::coding_agent::delegation::valid_identifier(request_key, 512) {
            return Err(CoordinatorError::Conflict);
        }
        let mut guard = RunLockLease::acquire(self.run_lock(run_id)).await;
        let run = self.require_run(run_id)?;
        if run.revision != expected_revision
            || run.state != RunState::AwaitingReview
            || run.snapshot.execution_mode != ExecutionMode::OneShot
            || !run.snapshot.semantic_judge_enabled
            || run.snapshot.approval.is_none()
        {
            return Err(CoordinatorError::Conflict);
        }
        let attempt_id = run
            .current_attempt_id
            .as_deref()
            .ok_or(CoordinatorError::Conflict)?;
        if self.has_cancellation_effect(run_id, attempt_id)? {
            return Err(CoordinatorError::Conflict);
        }
        let name = format!("human-semantic-{}", sha256_hex(request_key.as_bytes()));
        if let Some(record) =
            self.read_semantic_reassessment(run_id, expected_revision, issue_revision, request_key)?
        {
            return Ok(record);
        }
        let evidence = run
            .execution_evidence
            .as_ref()
            .ok_or(CoordinatorError::Conflict)?;
        if !assessment_is_successful(evidence) {
            return Err(CoordinatorError::Conflict);
        }
        let report_reference = evidence
            .report_reference
            .as_ref()
            .ok_or(CoordinatorError::Conflict)?;
        let report = self.inner.store.read_evidence(run_id, report_reference)?;
        self.validate_report_for_run(run_id, attempt_id, &report)
            .map_err(|_| CoordinatorError::Conflict)?;
        let execution = self
            .inner
            .execution
            .as_ref()
            .cloned()
            .ok_or(CoordinatorError::AdapterUnavailable)?;
        let identity = self.identity_for(run_id)?;
        guard.release();
        let source = execution
            .source_inspection(&identity)
            .await
            .map_err(|_| CoordinatorError::BackgroundFailure)?
            .ok_or(CoordinatorError::Conflict)?;
        if source.run_id != run_id.as_str()
            || source.attempt_id != attempt_id
            || source.base_commit != run.snapshot.base_commit
            || source.branch != run.snapshot.workspace_branch
            || !super::super::model::is_full_commit_id(&source.head_commit)
        {
            return Err(CoordinatorError::Conflict);
        }
        let source_sha256 =
            sha256_hex(&serde_json::to_vec(&source).map_err(|_| CoordinatorError::Conflict)?);
        guard.reacquire().await;
        let current = self.require_run(run_id)?;
        if current.revision != expected_revision
            || current.state != RunState::AwaitingReview
            || self.has_cancellation_effect(run_id, attempt_id)?
        {
            return Err(CoordinatorError::Conflict);
        }
        let intent_name = format!(
            "human-semantic-intent-{}",
            sha256_hex(request_key.as_bytes())
        );
        if let Some(record) =
            self.read_semantic_reassessment(run_id, expected_revision, issue_revision, request_key)?
        {
            return Ok(record);
        }
        let operation_key = format!("human-semantic:{}", sha256_hex(request_key.as_bytes()));
        let claim = self.inner.store.claim_effect(
            run_id,
            attempt_id,
            &operation_key,
            DurableOperationRequest::ReassessSemantics {
                expected_run_revision: expected_revision,
                issue_revision,
                source_sha256: source_sha256.clone(),
                report_reference: report_reference.clone(),
            },
        )?;
        // Only the transaction winner may dispatch. A distinct explicit human
        // request does not replay this potentially unknown operation.
        if !claim.newly_created {
            return Err(CoordinatorError::Conflict);
        }
        let mut record = SemanticReassessmentRecord {
            run_id: run_id.to_string(),
            attempt_id: attempt_id.to_owned(),
            request_key: request_key.to_owned(),
            expected_run_revision: expected_revision,
            issue_revision,
            source_sha256,
            source_head_commit: source.head_commit.clone(),
            source_is_clean: source.uncommitted_diff.as_deref() == Some(&[])
                && source.new_files.is_empty(),
            report_reference: report_reference.clone(),
            assessment: crate::coding_agent::semantic_judge::SemanticAssessment::inconclusive(
                "provider_result_unconfirmed",
            ),
        };
        self.write_unique_evidence(
            run_id,
            attempt_id,
            &intent_name,
            JSON_MEDIA_TYPE,
            &serde_json::to_vec(&record).map_err(|_| CoordinatorError::Conflict)?,
        )?;
        guard.release();
        record.assessment = execution
            .reassess_semantics(
                &identity,
                &source,
                &report,
                &evidence.validation,
                request_key,
            )
            .await
            .unwrap_or_else(|_| {
                crate::coding_agent::semantic_judge::SemanticAssessment::inconclusive(
                    "provider_result_unconfirmed",
                )
            });
        if execution
            .source_inspection(&identity)
            .await
            .ok()
            .flatten()
            .as_ref()
            != Some(&source)
        {
            record.assessment =
                crate::coding_agent::semantic_judge::SemanticAssessment::inconclusive(
                    "source_changed_during_reassessment",
                );
        }
        guard.reacquire().await;
        let current = self.require_run(run_id)?;
        if current.revision != expected_revision
            || current.state != RunState::AwaitingReview
            || self.has_cancellation_effect(run_id, attempt_id)?
        {
            return Err(CoordinatorError::Conflict);
        }
        let saved = self.write_unique_evidence(
            run_id,
            attempt_id,
            &name,
            JSON_MEDIA_TYPE,
            &serde_json::to_vec(&record).map_err(|_| CoordinatorError::Conflict)?,
        )?;
        self.inner.store.complete_operation(
            run_id,
            &operation_key,
            super::super::model::DurableOperationResult {
                outcome: DurableOperationOutcome::Succeeded,
                result_digest: Some(saved.sha256),
                evidence_ids: vec![saved.evidence_id],
            },
        )?;
        Ok(record)
    }
}
