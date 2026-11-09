use serde::{Deserialize, Serialize};

use super::{RunId, RunPurpose, store::CodingAgentError};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DelegationPlanRequest {
    pub issue_id: String,
    pub profile_id: String,
    pub purpose: RunPurpose,
    #[serde(default)]
    pub prompt: Option<String>,
}

impl DelegationPlanRequest {
    pub fn validate(&self) -> Result<(), CodingAgentError> {
        if !valid_identifier(&self.issue_id, 512)
            || !valid_identifier(&self.profile_id, 256)
            || self
                .prompt
                .as_ref()
                .is_some_and(|prompt| prompt.len() > 48 * 1024 || prompt.contains('\0'))
        {
            return Err(CodingAgentError::Invalid(
                "invalid coding-agent delegation request".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) fn valid_identifier(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DelegationPlanRecord {
    pub delegation_plan_id: String,
    pub request: DelegationPlanRequest,
    pub issue_revision: u64,
    pub settings_revision: u64,
    pub created_at_ms: u64,
    pub run_id: Option<RunId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DelegationStatusRequest {
    pub issue_id: String,
    pub delegation_plan_id: Option<String>,
    pub run_id: Option<RunId>,
}

impl DelegationStatusRequest {
    pub fn validate(&self) -> Result<(), CodingAgentError> {
        if !valid_identifier(&self.issue_id, 512)
            || self.delegation_plan_id.is_some() == self.run_id.is_some()
            || self
                .delegation_plan_id
                .as_ref()
                .is_some_and(|id| !valid_identifier(id, 512))
        {
            return Err(CodingAgentError::Invalid(
                "specify an issue and exactly one plan or run".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationStatus {
    pub delegation_plan: Option<DelegationPlanRecord>,
    pub run_id: Option<RunId>,
    pub state: Option<super::RunState>,
    pub report_references: Vec<super::EvidenceReference>,
}

pub fn read_delegation_status(
    store: &super::store::RunStore,
    request: &DelegationStatusRequest,
) -> Result<DelegationStatus, CodingAgentError> {
    request.validate()?;
    let delegation_plan = request
        .delegation_plan_id
        .as_ref()
        .map(|id| {
            store
                .delegation_plan(id, &request.issue_id)?
                .ok_or_else(|| CodingAgentError::NotFound("delegation plan for this issue".into()))
        })
        .transpose()?;
    let run_id = request.run_id.clone().or_else(|| {
        delegation_plan
            .as_ref()
            .and_then(|plan| plan.run_id.clone())
    });
    let mut status = DelegationStatus {
        delegation_plan,
        run_id: run_id.clone(),
        state: None,
        report_references: Vec::new(),
    };
    if let Some(run_id) = run_id {
        let run = store
            .get_run(&run_id)?
            .filter(|run| run.snapshot.issue_id == request.issue_id)
            .ok_or_else(|| CodingAgentError::NotFound("run for this issue".into()))?;
        status.state = Some(run.state);
        for reference in store.evidence(&run_id)? {
            if run.current_attempt_id.as_deref() == Some(reference.attempt_id.as_str())
                && matches!(
                    reference.name.as_str(),
                    "agent-report" | "issue-result-report"
                )
                && reference.save_confirmed
                && store.verify_evidence(&run_id, &reference.evidence_id)?
            {
                status.report_references.push(reference);
            }
        }
    }
    Ok(status)
}
