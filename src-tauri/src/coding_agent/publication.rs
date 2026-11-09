//! Host-only, versioned publication adapter contract.
//!
//! The adapter receives repository identity and immutable reviewed commit
//! facts, never credential bytes or a guest-controlled repository URL.

use std::collections::BTreeSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{AgentRunSnapshot, ExecutionResult, PublicationPlan};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicationSupport {
    Verified {
        executor_version: String,
        protocol_version: u32,
        features: BTreeSet<String>,
        approved_base_branch: String,
        approved_web_base_url: String,
    },
    Disabled(Vec<String>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PublicationRequest {
    pub run_id: String,
    pub attempt_id: String,
    pub repository_host: String,
    pub repository_owner: String,
    pub repository_name: String,
    pub approved_web_base_url: String,
    pub branch: String,
    pub base_branch: String,
    pub base_commit: String,
    pub frozen_commit: String,
    pub plan: PublicationPlan,
    pub operation_key: String,
    pub pull_request_title: Option<String>,
    pub pull_request_body: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum PublicationStart {
    Inline,
    Admitted { parent_job_id: Option<i64> },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PublicationOutcome {
    NoSourceChanges {
        source: coding_agent_executor::SourceInspection,
    },
    BranchPublished {
        branch: String,
        commit: String,
        remote_head: String,
    },
    PullRequestCreated {
        branch: String,
        head_commit: String,
        number: u64,
        url: String,
    },
    /// Legacy receipt shape without the tree-equality proof required for
    /// acceptance. Coordinators must fail closed on this outcome.
    NoChanges {
        existing_commit: String,
    },
    /// The executor reported its versioned base-vs-frozen tree comparison as
    /// equal and returned both observed tree identifiers.
    NoChangesWithTreeProof {
        base_commit: String,
        frozen_commit: String,
        base_tree_id: String,
        frozen_tree_id: String,
    },
    /// The remote action may have happened, but no trustworthy receipt arrived.
    Unknown,
    Rejected,
}

pub(crate) fn source_has_no_changes(source: &coding_agent_executor::SourceInspection) -> bool {
    source.tracked_diff.is_empty()
        && source.uncommitted_diff.as_ref().is_some_and(Vec::is_empty)
        && source.new_files.is_empty()
}

pub(crate) fn no_changes_receipt_matches(
    source: &coding_agent_executor::SourceInspection,
    run_id: &str,
    attempt_id: &str,
    branch: &str,
    base_commit: &str,
    frozen_commit: &str,
) -> bool {
    source_has_no_changes(source)
        && source.run_id == run_id
        && source.attempt_id == attempt_id
        && source.branch == branch
        && source.base_commit == base_commit
        && source.head_commit == frozen_commit
        && super::model::is_full_commit_id(base_commit)
        && super::model::is_full_commit_id(frozen_commit)
}

#[async_trait]
#[allow(clippy::double_must_use)] // async_trait's boxed future is already must-use.
pub trait PublicationAdapter: Send + Sync {
    async fn preflight(
        &self,
        snapshot: &AgentRunSnapshot,
    ) -> Result<PublicationSupport, ExecutionResult>;

    /// Admit an operation separately from its result wait. Legacy adapters
    /// continue to perform both phases inside `publish`.
    async fn begin_publication(
        &self,
        _request: &PublicationRequest,
    ) -> Result<PublicationStart, ExecutionResult> {
        Ok(PublicationStart::Inline)
    }

    /// Called only for a newly-created durable publication intent.
    async fn publish(
        &self,
        request: &PublicationRequest,
    ) -> Result<PublicationOutcome, ExecutionResult>;

    /// Reconcile an existing durable intent; implementations must not repeat
    /// the remote mutation from this method.
    async fn reconcile(
        &self,
        request: &PublicationRequest,
    ) -> Result<PublicationOutcome, ExecutionResult>;
}

pub(crate) fn validate_publication_support(
    snapshot: &AgentRunSnapshot,
    support: &PublicationSupport,
) -> Vec<String> {
    let PublicationSupport::Verified {
        executor_version,
        protocol_version,
        features,
        approved_base_branch,
        approved_web_base_url,
    } = support
    else {
        return match support {
            PublicationSupport::Disabled(blockers) if blockers.is_empty() => {
                vec!["publication_support_disabled".to_owned()]
            }
            PublicationSupport::Disabled(blockers) => blockers.clone(),
            PublicationSupport::Verified { .. } => unreachable!(),
        };
    };
    let mut blockers = Vec::new();
    if executor_version.trim().is_empty()
        || *protocol_version != coding_agent_executor::PROTOCOL_VERSION
        || !super::is_valid_branch_name(approved_base_branch)
        || !valid_web_base_url(approved_web_base_url, &snapshot.repository.identity.host)
    {
        blockers.push("verified_publication_executor_version".to_owned());
    }
    let required: &[&str] = match &snapshot.publication_plan {
        PublicationPlan::NoPublication => &[],
        PublicationPlan::PushBranch => &["push"],
        PublicationPlan::CreatePullRequest => &["push", "pull_request"],
    };
    blockers.extend(
        required
            .iter()
            .filter(|feature| !features.contains(**feature))
            .map(|feature| format!("verified_publication_feature:{feature}")),
    );
    blockers
}

fn valid_web_base_url(value: &str, expected_host: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    let Ok(expected) = url::Url::parse(&format!("https://{expected_host}/")) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == expected.host_str()
        && url.port_or_known_default() == expected.port_or_known_default()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}
