use coding_agent_executor::{
    CaptureGuarantee, EXECUTOR_VERSION, PROTOCOL_VERSION, RepositoryId, ResolvedArtifact,
};
use lookback_tauri_lib::coding_agent::{
    ExecutionIdentity, RepositoryIdentity, ValidationTargetSnapshot,
    execution::{SealedArtifactBinding, validate_resolved_sealed_artifact},
};

fn fixture() -> (
    ExecutionIdentity,
    RepositoryIdentity,
    SealedArtifactBinding,
    ValidationTargetSnapshot,
    ResolvedArtifact,
) {
    let identity = ExecutionIdentity {
        run_id: "run-verified".into(),
        attempt_id: "attempt-1".into(),
        workflow_execution_id: "workflow-1".into(),
        parent_job_id: "parent-1".into(),
        child_sandbox_job_id: "child-1".into(),
        worker_id: 1,
        sandbox_job_id: "sandbox-1".into(),
    };
    let repository = RepositoryIdentity::new("forge.example", "org", "repo").unwrap();
    let target =
        ValidationTargetSnapshot::new("b".repeat(40), "c".repeat(40), "d".repeat(64)).unwrap();
    let binding = SealedArtifactBinding {
        full_commit_id: target.full_commit_id().to_owned(),
        full_tree_id: target.full_tree_id().to_owned(),
        untracked_manifest_digest: target.untracked_manifest_digest().to_owned(),
        target_snapshot_digest: target.digest().to_owned(),
    };
    let resolved = ResolvedArtifact {
        run_id: identity.run_id.clone(),
        attempt_id: identity.attempt_id.clone(),
        repository: RepositoryId {
            host: repository.host.clone(),
            owner: repository.organization.clone(),
            name: repository.repository.clone(),
        },
        branch: "agent/run-verified".into(),
        base_branch: "main".into(),
        base_commit: "a".repeat(40),
        approved_snapshot_digest: "e".repeat(64),
        scope_digest: "f".repeat(64),
        intent_approval_digest: "1".repeat(64),
        manifest_digest: "2".repeat(64),
        finalized_bytes_digest: "3".repeat(64),
        tree: target.full_tree_id().to_owned(),
        commit: target.full_commit_id().to_owned(),
        artifact_digest: "4".repeat(64),
        protocol_version: PROTOCOL_VERSION,
        executor_version: EXECUTOR_VERSION.into(),
        capture_guarantee: CaptureGuarantee::HostIsolatedAfterSeal,
    };
    (identity, repository, binding, target, resolved)
}

#[test]
fn native_resolve_must_match_the_exact_run_attempt_and_frozen_target() {
    let (identity, repository, binding, target, resolved) = fixture();
    assert!(
        validate_resolved_sealed_artifact(
            &identity,
            &repository,
            "agent/run-verified",
            "a".repeat(40).as_str(),
            "e".repeat(64).as_str(),
            &binding,
            &target,
            &resolved,
        )
        .is_ok()
    );

    let mut changed_attempt = resolved.clone();
    changed_attempt.attempt_id = "another-attempt".into();
    assert!(
        validate_resolved_sealed_artifact(
            &identity,
            &repository,
            "agent/run-verified",
            "a".repeat(40).as_str(),
            "e".repeat(64).as_str(),
            &binding,
            &target,
            &changed_attempt,
        )
        .is_err()
    );

    let mut changed_target = binding.clone();
    changed_target.untracked_manifest_digest = "5".repeat(64);
    assert!(
        validate_resolved_sealed_artifact(
            &identity,
            &repository,
            "agent/run-verified",
            "a".repeat(40).as_str(),
            "e".repeat(64).as_str(),
            &changed_target,
            &target,
            &resolved,
        )
        .is_err()
    );

    let mut changed_base = resolved.clone();
    changed_base.base_commit = "6".repeat(40);
    assert!(
        validate_resolved_sealed_artifact(
            &identity,
            &repository,
            "agent/run-verified",
            "a".repeat(40).as_str(),
            "e".repeat(64).as_str(),
            &binding,
            &target,
            &changed_base,
        )
        .is_err()
    );
}
