use lookback_tauri_lib::{
    coding_agent::{
        ExecutionMode, PublicationPlan, RunPurpose,
        assessment::ProviderSelection,
        model::{ApprovedJudgeInputField, CriterionEvaluationMethod},
    },
    commands::coding_agent::{
        CodingAgentCriterionPolicyRequest, CodingAgentRunPreview, CodingAgentRunPreviewRequest,
        CodingAgentValidationCommandRequest, CodingAgentValidationDraftRequest,
        build_approval_assessment_contract,
    },
};

fn provider() -> ProviderSelection {
    ProviderSelection {
        provider_id: "openai".into(),
        model_id: "gpt-4.1-2025-04-14".into(),
        model_version: "gpt-4.1-2025-04-14".into(),
        config_version: "a".repeat(64),
    }
}

#[test]
fn approval_contract_preserves_canonical_criteria_and_builds_the_native_preview_shape() {
    let acceptance = vec![
        "first line\nsecond line".into(),
        "follow-up condition".into(),
    ];
    let contract = build_approval_assessment_contract(
        RunPurpose::Implementation,
        acceptance.clone(),
        "issue-42",
        None,
        None,
        vec![CodingAgentValidationDraftRequest {
            required: true,
            command: None,
        }],
        Some(provider()),
    )
    .expect("canonical Issue criteria should produce an approval contract");

    assert_eq!(
        contract
            .criteria
            .iter()
            .map(|criterion| criterion.statement.clone())
            .collect::<Vec<_>>(),
        acceptance,
        "the native vector, including embedded newlines and order, is authoritative"
    );
    assert_eq!(contract.criteria[0].criterion_id, "AC-1");
    assert_eq!(contract.criteria[1].criterion_id, "AC-2");
    assert!(contract.criteria.iter().all(|criterion| criterion.required));
    assert!(
        contract
            .criteria
            .iter()
            .all(|criterion| { criterion.method == CriterionEvaluationMethod::SemanticJudge })
    );
    assert_eq!(contract.validation_plan[0].validation_id, "V-1");
    assert!(contract.validation_plan[0].required);
    assert!(contract.validation_plan[0].command.is_none());
    assert_eq!(contract.provider, Some(provider()));
    assert_eq!(
        contract.prompt_policy_version.as_deref(),
        Some("coding-agent-judge-v1")
    );
    assert!(
        contract
            .input_scope
            .allowed_sources
            .iter()
            .any(|source| { source.identity == "issue:issue-42" })
    );
    assert!(contract.input_scope.allowed_sources.iter().any(|source| {
        source.identity == "agent-report"
            && source.kind
                == lookback_tauri_lib::coding_agent::assessment::EvidenceSourceKind::Report
    }));
    assert!(contract.input_scope.allowed_sources.iter().any(|source| {
        source.identity == "V-1"
            && source.kind
                == lookback_tauri_lib::coding_agent::assessment::EvidenceSourceKind::ValidationResult
    }));
    assert!(
        contract
            .input_scope
            .allowed_fields
            .contains(&ApprovedJudgeInputField::IssueTitle)
    );
    assert!(
        contract
            .input_scope
            .allowed_fields
            .contains(&ApprovedJudgeInputField::IssueBody)
    );
    assert!(
        !contract
            .input_scope
            .allowed_fields
            .contains(&ApprovedJudgeInputField::RepositorySummary)
    );
    assert!(
        !contract
            .input_scope
            .allowed_fields
            .contains(&ApprovedJudgeInputField::ThreadGroupContext)
    );

    let dto = serde_json::to_value(&contract).unwrap();
    assert_eq!(dto["criteria"][0]["statement"], "first line\nsecond line");
    assert_eq!(dto["criteria"][0]["method"], "semantic_judge");
    assert_eq!(dto["validation_plan"][0]["validation_id"], "V-1");
    assert_eq!(dto["input_scope"]["max_input_bytes"], 128 * 1024);
    assert_eq!(dto["judge_input_scope_digest"].as_str().unwrap().len(), 64);
    assert_eq!(dto["provider"]["model_version"], "gpt-4.1-2025-04-14");
    assert_eq!(contract.digest().unwrap().len(), 64);
}

#[test]
fn missing_provider_defaults_each_condition_to_required_unknown_without_implicit_skip() {
    let contract = build_approval_assessment_contract(
        RunPurpose::Investigation,
        vec!["retain this unverified condition".into()],
        "issue-43",
        None,
        None,
        Vec::new(),
        None,
    )
    .unwrap();

    assert_eq!(contract.purpose, RunPurpose::Investigation);
    assert_eq!(contract.provider, None);
    assert!(contract.criteria[0].required);
    assert_eq!(
        contract.criteria[0].method,
        CriterionEvaluationMethod::Unknown
    );
    assert!(contract.has_unknown_criterion_methods());
    assert!(!contract.can_dispatch_semantic_assessment());
    assert!(
        !contract
            .input_scope
            .allowed_fields
            .contains(&ApprovedJudgeInputField::Report)
    );
}

#[test]
fn criteria_policy_ids_must_match_the_complete_canonical_ac_sequence() {
    let policy = |criterion_id: &str| CodingAgentCriterionPolicyRequest {
        criterion_id: criterion_id.into(),
        required: true,
        method: CriterionEvaluationMethod::Deterministic,
    };
    let build = |policies| {
        build_approval_assessment_contract(
            RunPurpose::Implementation,
            vec!["one".into(), "two".into()],
            "issue-44",
            None,
            Some(policies),
            Vec::new(),
            None,
        )
    };

    assert!(build(vec![policy("AC-1"), policy("AC-2")]).is_ok());
    assert!(build(vec![policy("AC-1"), policy("AC-1")]).is_err());
    assert!(build(vec![policy("AC-1"), policy("AC-3")]).is_err());
    assert!(build(vec![policy("AC-2"), policy("AC-1")]).is_err());
    assert!(build(vec![policy("AC-1")]).is_err());
    assert!(build(vec![policy("AC-1"), policy("AC-2"), policy("AC-3")]).is_err());
}

#[test]
fn caller_supplied_validation_command_is_rejected_without_a_native_validation_profile() {
    let result = build_approval_assessment_contract(
        RunPurpose::Implementation,
        vec!["one".into()],
        "issue-45",
        None,
        None,
        vec![CodingAgentValidationDraftRequest {
            required: true,
            command: Some(CodingAgentValidationCommandRequest {
                argv: vec!["cargo".into(), "test".into()],
                working_directory: "/workspace".into(),
                timeout_ms: 30_000,
            }),
        }],
        None,
    );

    assert_eq!(
        result.unwrap_err(),
        "validation_profile_unavailable",
        "a bounded UI command is not a trusted host profile"
    );
    let outside_workspace = build_approval_assessment_contract(
        RunPurpose::Implementation,
        vec!["one".into()],
        "issue-45",
        None,
        None,
        vec![CodingAgentValidationDraftRequest {
            required: true,
            command: Some(CodingAgentValidationCommandRequest {
                argv: vec!["cargo".into(), "test".into()],
                working_directory: "/workspace/../host".into(),
                timeout_ms: 30_000,
            }),
        }],
        None,
    );
    assert_eq!(outside_workspace.unwrap_err(), "validation_command_invalid");
}

#[test]
fn validation_profile_permissions_and_digest_are_not_client_supplied_dto_fields() {
    let request: Result<CodingAgentRunPreviewRequest, _> = serde_json::from_value(
        serde_json::json!({
            "issueId": "issue-49",
            "profileId": "profile-1",
            "executionMode": "one_shot",
            "purpose": "implementation",
            "publicationPlan": "no_publication",
            "validationDrafts": [{
                "required": true,
                "command": {
                    "argv": ["cargo", "test"],
                    "workingDirectory": "/workspace",
                    "timeoutMs": 30000,
                    "permissions": ["read_workspace"],
                    "profileDigest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                }
            }]
        }),
    );

    assert!(
        request.is_err(),
        "profile permissions and digests are host-owned"
    );
}

#[test]
fn approved_method_requiredness_provider_and_scope_all_participate_in_the_contract_digest() {
    let build = |required, method, selected_provider, issue_id: &str| {
        build_approval_assessment_contract(
            RunPurpose::Implementation,
            vec!["same canonical statement".into()],
            issue_id,
            None,
            Some(vec![CodingAgentCriterionPolicyRequest {
                criterion_id: "AC-1".into(),
                required,
                method,
            }]),
            Vec::new(),
            selected_provider,
        )
        .unwrap()
    };
    let baseline = build(
        true,
        CriterionEvaluationMethod::SemanticJudge,
        Some(provider()),
        "issue-46",
    );
    let changed_requiredness = build(
        false,
        CriterionEvaluationMethod::SemanticJudge,
        Some(provider()),
        "issue-46",
    );
    let changed_method = build(
        true,
        CriterionEvaluationMethod::Unknown,
        Some(provider()),
        "issue-46",
    );
    let changed_provider = build(
        true,
        CriterionEvaluationMethod::SemanticJudge,
        Some(ProviderSelection {
            config_version: "b".repeat(64),
            ..provider()
        }),
        "issue-46",
    );
    let changed_scope = build(
        true,
        CriterionEvaluationMethod::SemanticJudge,
        Some(provider()),
        "issue-47",
    );

    let digest = baseline.digest().unwrap();
    assert_ne!(digest, changed_requiredness.digest().unwrap());
    assert_ne!(digest, changed_method.digest().unwrap());
    assert_ne!(digest, changed_provider.digest().unwrap());
    assert_ne!(digest, changed_scope.digest().unwrap());
}

#[test]
fn preview_dto_uses_camel_case_at_the_command_boundary_and_native_contract_keys_inside() {
    let request: CodingAgentRunPreviewRequest = serde_json::from_value(serde_json::json!({
        "issueId": "issue-48",
        "profileId": "profile-1",
        "executionMode": "one_shot",
        "purpose": "implementation",
        "publicationPlan": "no_publication",
        "criterionPolicies": [{
            "criterionId": "AC-1",
            "required": false,
            "method": "deterministic"
        }],
        "validationDrafts": [{"required": true, "command": null}]
    }))
    .unwrap();
    let contract = build_approval_assessment_contract(
        request.purpose,
        vec!["condition".into()],
        &request.issue_id,
        None,
        request.criterion_policies,
        request.validation_drafts,
        Some(provider()),
    )
    .unwrap();
    let preview = CodingAgentRunPreview {
        plan_id: "plan".into(),
        digest: "digest".into(),
        issue_id: "issue-48".into(),
        issue_revision: 1,
        issue_title: "Issue title".into(),
        issue_state: "open".into(),
        source_labels: Vec::new(),
        settings_revision: 1,
        profile_id: Some("profile-1".into()),
        profile_version: Some(1),
        execution_mode: ExecutionMode::OneShot,
        purpose: RunPurpose::Implementation,
        publication_plan: PublicationPlan::NoPublication,
        selected_repository: None,
        repository_selection_reason: None,
        repository_candidates: Vec::new(),
        auto_done_requested: false,
        assessment_contract_digest: contract.digest().unwrap(),
        assessment_provider_supported: true,
        assessment_contract: contract,
        blockers: vec!["verified_execution_adapter_unavailable".into()],
        can_approve: false,
        base_commit: None,
        workspace_branch: None,
        image_digest: None,
        preparation_key: None,
    };
    let dto = serde_json::to_value(preview).unwrap();

    assert!(dto.get("assessmentContract").is_some());
    assert!(dto.get("assessmentContractDigest").is_some());
    assert_eq!(
        dto["assessmentContract"]["schema_version"],
        serde_json::json!(1)
    );
    assert_eq!(
        dto["assessmentContract"]["criteria"][0]["required"],
        serde_json::json!(false)
    );
    assert_eq!(dto["assessmentProviderSupported"], true);
    assert_eq!(dto["canApprove"], false);
    assert!(dto.get("llmSettings").is_none());
    assert!(dto.get("llmSelectionDigest").is_none());
}

#[test]
fn native_validation_commands_are_bound_to_the_host_selected_profile() {
    use lookback_tauri_lib::commands::coding_agent::build_profile_approval_assessment_contract;
    let build = |version| {
        build_profile_approval_assessment_contract(
            RunPurpose::Implementation,
            vec![],
            "issue-45",
            None,
            None,
            vec![CodingAgentValidationDraftRequest {
                required: true,
                command: Some(CodingAgentValidationCommandRequest {
                    argv: vec![
                        "cargo".into(),
                        "test".into(),
                        "--".into(),
                        "literal $(never)\nnext".into(),
                        String::new(),
                    ],
                    working_directory: "/workspace/run/crate".into(),
                    timeout_ms: 30_000,
                }),
            }],
            None,
            Some(("profile", version)),
        )
    };
    let contract = build(3).unwrap();
    let command = contract.validation_plan[0].command.as_ref().unwrap();
    assert_eq!(command.argv[3], "literal $(never)\nnext");
    assert_eq!(command.argv[4], "");
    assert_eq!(command.working_directory, "/workspace/run/crate");
    assert_ne!(
        command.profile_digest,
        build(4).unwrap().validation_plan[0]
            .command
            .as_ref()
            .unwrap()
            .profile_digest
    );
    assert!(build(0).is_err());
}

#[test]
fn delegated_approval_requires_the_selected_unstarted_plan_and_matching_human_preview() {
    use lookback_tauri_lib::coding_agent::delegation::{
        DelegationPlanRecord, DelegationPlanRequest,
    };
    use lookback_tauri_lib::commands::coding_agent::validate_delegation_plan_for_approval;
    let preview: CodingAgentRunPreviewRequest = serde_json::from_value(serde_json::json!({
        "issueId":"issue", "profileId":"profile", "executionMode":"one_shot",
        "purpose":"implementation", "publicationPlan":"no_publication", "prompt":"requested task"
    }))
    .unwrap();
    let plan = DelegationPlanRecord {
        delegation_plan_id: "plan".into(),
        request: DelegationPlanRequest {
            issue_id: preview.issue_id.clone(),
            profile_id: preview.profile_id.clone(),
            purpose: preview.purpose,
            prompt: preview.prompt.clone(),
        },
        issue_revision: 4,
        settings_revision: 3,
        created_at_ms: 1,
        run_id: None,
    };
    assert!(validate_delegation_plan_for_approval(&plan, &preview).is_ok());
    for field in ["issue", "profile", "purpose", "prompt", "linked"] {
        let mut changed = plan.clone();
        match field {
            "issue" => changed.request.issue_id = "another".into(),
            "profile" => changed.request.profile_id = "another".into(),
            "purpose" => changed.request.purpose = RunPurpose::Investigation,
            "prompt" => changed.request.prompt = Some("another task".into()),
            "linked" => {
                changed.run_id =
                    Some(lookback_tauri_lib::coding_agent::RunId::new("started-run").unwrap())
            }
            _ => unreachable!(),
        }
        assert!(
            validate_delegation_plan_for_approval(&changed, &preview).is_err(),
            "{field}"
        );
    }
}
