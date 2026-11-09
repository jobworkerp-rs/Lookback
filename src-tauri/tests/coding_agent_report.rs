use std::collections::BTreeMap;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::Path;

use lookback_tauri_lib::coding_agent::report::{
    AgentReportStatus, CompletionAssessment, MAX_EVIDENCE_REFERENCES, MAX_IDENTIFIER_BYTES,
    MAX_REPORT_BYTES, MAX_REPORT_ITEMS, MAX_TEXT_BYTES, ReportExpectation, ReportRejection,
    TaskKind, ValidationClaim, read_report, report_relative_path, validate_report,
};

const VALID_INVESTIGATION: &str = r#"{
  "schema_version": 1,
  "run_id": "run-123",
  "attempt_id": "attempt-1",
  "task_kind": "investigation",
  "status": "completed",
  "summary": "Located the relevant call path.",
  "acceptance": [
    {"criterion_id": "AC-1", "assessment": "met", "evidence": ["repo:src/lib.rs#L4"]}
  ],
  "validation": [
    {"validation_id": "V-1", "claimed_result": "not_run", "evidence": [], "reason": "Investigation only."}
  ],
  "investigation": {
    "scope": ["repo:src/lib.rs"],
    "findings": [
      {"summary": "The call originates in the library entry point.", "criterion_ids": ["AC-1"], "evidence": ["repo:src/lib.rs#L4"]}
    ],
    "limitations": [],
    "response_questions": []
  },
  "implementation": null,
  "unresolved": []
}"#;

fn investigation_expectation() -> ReportExpectation {
    ReportExpectation::new(
        "run-123",
        "attempt-1",
        TaskKind::Investigation,
        BTreeMap::from([("AC-1".to_owned(), true)]),
        BTreeMap::from([("V-1".to_owned(), false)]),
    )
    .unwrap()
}

fn json_value() -> serde_json::Value {
    serde_json::from_str(VALID_INVESTIGATION).unwrap()
}

fn validate_value(
    value: &serde_json::Value,
    expectation: &ReportExpectation,
) -> Result<lookback_tauri_lib::coding_agent::report::ValidatedReport, ReportRejection> {
    validate_report(&serde_json::to_vec(value).unwrap(), expectation)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_expected_report(workspace: &Path, bytes: &[u8]) {
    let report_path = workspace.join(report_relative_path("attempt-1").unwrap());
    std::fs::create_dir_all(report_path.parent().unwrap()).unwrap();
    std::fs::write(report_path, bytes).unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reads_the_fixed_investigation_report_and_keeps_agent_claims_untrusted() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let report_path = workspace.join(report_relative_path("attempt-1").unwrap());
    std::fs::create_dir_all(report_path.parent().unwrap()).unwrap();
    std::fs::write(&report_path, VALID_INVESTIGATION.as_bytes()).unwrap();

    let result = read_report(&workspace, &investigation_expectation());

    assert!(result.rejection.is_none());
    assert!(result.bytes_complete);
    assert_eq!(result.raw_bytes, VALID_INVESTIGATION.as_bytes());
    assert_eq!(
        result.raw_bytes_sha256, "fa026db0007c19bde0c99f1b2c0c9cc0fd0aa13c1c145fafc8d493cb98552944",
        "digest fixture was calculated independently with Python hashlib"
    );
    let validated = result.validated_report.unwrap();
    assert_eq!(validated.report.status, AgentReportStatus::Completed);
    assert_eq!(
        validated.assessment.completion,
        CompletionAssessment::Completed
    );
    assert_eq!(
        validated.assessment.required_validation_claims_not_passed,
        Vec::<String>::new(),
        "an optional not_run claim must not be confused with a required failure"
    );
    assert_eq!(
        validated.report.validation[0].claimed_result,
        ValidationClaim::NotRun,
        "the module preserves the agent claim without turning it into a receipt"
    );
}

#[test]
fn validates_an_implementation_report_against_its_own_approval_snapshot() {
    let mut value = json_value();
    value["task_kind"] = "implementation".into();
    value["investigation"] = serde_json::Value::Null;
    value["implementation"] = serde_json::json!({
        "change_summary": "Updated the entry point.",
        "changed_files": ["src/lib.rs"],
        "implementation_notes": []
    });
    let expectation = ReportExpectation::new(
        "run-123",
        "attempt-1",
        TaskKind::Implementation,
        BTreeMap::from([("AC-1".to_owned(), true)]),
        BTreeMap::from([("V-1".to_owned(), false)]),
    )
    .unwrap();

    let validated = validate_value(&value, &expectation).unwrap();

    assert!(validated.report.investigation.is_none());
    assert_eq!(
        validated.report.implementation.unwrap().changed_files,
        vec!["src/lib.rs"]
    );
}

#[test]
fn rejects_duplicate_unknown_missing_and_wrongly_typed_schema_fields() {
    let expectation = investigation_expectation();
    let mut unknown_top = json_value();
    unknown_top["unapproved"] = true.into();
    let mut unknown_nested = json_value();
    unknown_nested["investigation"]["unapproved"] = true.into();
    let mut missing_required_nullable = json_value();
    missing_required_nullable
        .as_object_mut()
        .unwrap()
        .remove("implementation");
    let mut missing_required_text = json_value();
    missing_required_text
        .as_object_mut()
        .unwrap()
        .remove("summary");
    let mut wrong_type = json_value();
    wrong_type["summary"] = 7.into();

    let malformed = [
        (
            serde_json::to_vec(&unknown_top).unwrap(),
            ReportRejection::InvalidJson,
        ),
        (
            serde_json::to_vec(&unknown_nested).unwrap(),
            ReportRejection::InvalidJson,
        ),
        (
            serde_json::to_vec(&missing_required_nullable).unwrap(),
            ReportRejection::InvalidJson,
        ),
        (
            serde_json::to_vec(&missing_required_text).unwrap(),
            ReportRejection::InvalidJson,
        ),
        (
            serde_json::to_vec(&wrong_type).unwrap(),
            ReportRejection::InvalidJson,
        ),
        (
            VALID_INVESTIGATION
                .replace(
                    "\"schema_version\": 1,",
                    "\"schema_version\": 1, \"schema_version\": 1,",
                )
                .into_bytes(),
            ReportRejection::InvalidJson,
        ),
        (
            VALID_INVESTIGATION
                .replace(
                    "\"assessment\": \"met\"",
                    "\"assessment\": \"met\", \"assessment\": \"unmet\"",
                )
                .into_bytes(),
            ReportRejection::InvalidJson,
        ),
        (b"{broken".to_vec(), ReportRejection::InvalidJson),
        (vec![0xff, 0xfe], ReportRejection::InvalidUtf8),
    ];
    for (malformed, expected_rejection) in malformed {
        assert_eq!(
            validate_report(&malformed, &expectation),
            Err(expected_rejection),
            "strict schema rejection for {:?}",
            String::from_utf8_lossy(&malformed)
        );
    }
}

#[test]
fn rejects_wrong_version_run_attempt_task_and_task_specific_null_shape() {
    let expectation = investigation_expectation();
    let mut version = json_value();
    version["schema_version"] = 2.into();
    assert_eq!(
        validate_value(&version, &expectation).unwrap_err(),
        ReportRejection::UnsupportedSchemaVersion
    );

    let mut wrong_run = json_value();
    wrong_run["run_id"] = "other-run".into();
    assert_eq!(
        validate_value(&wrong_run, &expectation).unwrap_err(),
        ReportRejection::RunIdMismatch
    );
    let mut wrong_attempt = json_value();
    wrong_attempt["attempt_id"] = "attempt-2".into();
    assert_eq!(
        validate_value(&wrong_attempt, &expectation).unwrap_err(),
        ReportRejection::AttemptIdMismatch
    );
    let mut wrong_task = json_value();
    wrong_task["task_kind"] = "implementation".into();
    assert_eq!(
        validate_value(&wrong_task, &expectation).unwrap_err(),
        ReportRejection::TaskKindMismatch
    );
    let mut missing_investigation_object = json_value();
    missing_investigation_object["investigation"] = serde_json::Value::Null;
    assert_eq!(
        validate_value(&missing_investigation_object, &expectation).unwrap_err(),
        ReportRejection::TaskKindMismatch
    );
}

#[test]
fn requires_exactly_one_of_every_approved_acceptance_and_validation_id() {
    let expectation = investigation_expectation();
    let mut missing_acceptance = json_value();
    missing_acceptance["acceptance"] = serde_json::json!([]);
    let mut unknown_acceptance = json_value();
    unknown_acceptance["acceptance"][0]["criterion_id"] = "AC-unknown".into();
    let mut duplicate_acceptance = json_value();
    duplicate_acceptance["acceptance"] = serde_json::json!([
        duplicate_acceptance["acceptance"][0].clone(),
        duplicate_acceptance["acceptance"][0].clone()
    ]);
    for value in [missing_acceptance, unknown_acceptance, duplicate_acceptance] {
        assert_eq!(
            validate_value(&value, &expectation).unwrap_err(),
            ReportRejection::AcceptanceIdMismatch
        );
    }

    let mut missing_validation = json_value();
    missing_validation["validation"] = serde_json::json!([]);
    let mut unknown_validation = json_value();
    unknown_validation["validation"][0]["validation_id"] = "V-unknown".into();
    let mut duplicate_validation = json_value();
    duplicate_validation["validation"] = serde_json::json!([
        duplicate_validation["validation"][0].clone(),
        duplicate_validation["validation"][0].clone()
    ]);
    for value in [missing_validation, unknown_validation, duplicate_validation] {
        assert_eq!(
            validate_value(&value, &expectation).unwrap_err(),
            ReportRejection::ValidationIdMismatch
        );
    }
}

#[test]
fn reports_untrusted_completion_consistency_and_required_validation_claim_gaps_separately() {
    let expectation = ReportExpectation::new(
        "run-123",
        "attempt-1",
        TaskKind::Investigation,
        BTreeMap::from([("AC-1".to_owned(), true)]),
        BTreeMap::from([("V-1".to_owned(), true)]),
    )
    .unwrap();
    let optional_validation_expectation = investigation_expectation();

    let valid_required_not_run =
        validate_report(VALID_INVESTIGATION.as_bytes(), &expectation).unwrap();
    assert_eq!(
        valid_required_not_run.assessment.completion,
        CompletionAssessment::IncompleteRequiredValidation,
        "a required not_run claim cannot support a completed report assessment"
    );
    assert_eq!(
        valid_required_not_run.report.status,
        AgentReportStatus::Completed,
        "the typed report preserves the agent's original, untrusted status"
    );
    assert_eq!(
        valid_required_not_run
            .assessment
            .required_validation_claims_not_passed,
        vec!["V-1"]
    );
    let optional_not_run = validate_report(
        VALID_INVESTIGATION.as_bytes(),
        &optional_validation_expectation,
    )
    .unwrap();
    assert!(
        optional_not_run
            .assessment
            .required_validation_claims_not_passed
            .is_empty()
    );
    assert_eq!(
        optional_not_run.assessment.completion,
        CompletionAssessment::Completed
    );

    let mut unmet = json_value();
    unmet["acceptance"][0]["assessment"] = "unmet".into();
    let assessed = validate_value(&unmet, &expectation).unwrap();
    assert_eq!(
        assessed.assessment.required_acceptance_not_met_or_unknown,
        vec!["AC-1"]
    );
    assert_eq!(
        assessed.assessment.completion,
        CompletionAssessment::Inconsistent
    );

    let mut unknown = json_value();
    unknown["acceptance"][0]["assessment"] = "unknown".into();
    assert_eq!(
        validate_value(&unknown, &expectation)
            .unwrap()
            .assessment
            .completion,
        CompletionAssessment::Inconsistent
    );

    let optional_expectation = ReportExpectation::new(
        "run-123",
        "attempt-1",
        TaskKind::Investigation,
        BTreeMap::from([("AC-1".to_owned(), false)]),
        BTreeMap::from([("V-1".to_owned(), false)]),
    )
    .unwrap();
    assert_eq!(
        validate_value(&unmet, &optional_expectation)
            .unwrap()
            .assessment
            .completion,
        CompletionAssessment::Completed,
        "an optional unmet criterion does not invalidate the completion claim"
    );

    for (status, expected) in [
        ("partial", CompletionAssessment::Partial),
        ("blocked", CompletionAssessment::Blocked),
    ] {
        let mut declared_status = json_value();
        declared_status["status"] = status.into();
        assert_eq!(
            validate_value(&declared_status, &optional_expectation)
                .unwrap()
                .assessment
                .completion,
            expected
        );
    }

    let mut blocked = json_value();
    blocked["unresolved"] = serde_json::json!([{
        "description": "The required source is unavailable.",
        "blocks_completion": true,
        "criterion_ids": ["AC-1"]
    }]);
    let assessed = validate_value(&blocked, &expectation).unwrap();
    assert_eq!(assessed.assessment.blocking_unresolved_items, vec![0]);
    assert_eq!(
        assessed.assessment.completion,
        CompletionAssessment::Inconsistent
    );
}

#[test]
fn rejects_unknown_acceptance_references_but_preserves_known_references() {
    let expectation = investigation_expectation();
    let mut unknown_finding = json_value();
    unknown_finding["investigation"]["findings"][0]["criterion_ids"] =
        serde_json::json!(["AC-unknown"]);
    assert_eq!(
        validate_value(&unknown_finding, &expectation).unwrap_err(),
        ReportRejection::InvalidCriterionReference
    );
    let mut unknown_unresolved = json_value();
    unknown_unresolved["unresolved"] = serde_json::json!([{
        "description": "An open question.",
        "blocks_completion": false,
        "criterion_ids": ["AC-unknown"]
    }]);
    assert_eq!(
        validate_value(&unknown_unresolved, &expectation).unwrap_err(),
        ReportRejection::InvalidCriterionReference
    );
}

#[test]
fn evidence_references_use_the_normal_text_limit_not_the_identifier_limit() {
    let expectation = investigation_expectation();
    for location in ["acceptance", "validation", "finding"] {
        for reference in ["x".repeat(257), "é".repeat(MAX_TEXT_BYTES / 2)] {
            let mut value = json_value();
            match location {
                "acceptance" => value["acceptance"][0]["evidence"] = serde_json::json!([reference]),
                "validation" => value["validation"][0]["evidence"] = serde_json::json!([reference]),
                "finding" => {
                    value["investigation"]["findings"][0]["evidence"] =
                        serde_json::json!([reference]);
                }
                _ => unreachable!(),
            }
            assert!(
                validate_value(&value, &expectation).is_ok(),
                "{location} evidence of an allowed UTF-8 byte length"
            );
        }

        let reference = format!("{}a", "é".repeat(MAX_TEXT_BYTES / 2));
        let mut value = json_value();
        match location {
            "acceptance" => value["acceptance"][0]["evidence"] = serde_json::json!([reference]),
            "validation" => value["validation"][0]["evidence"] = serde_json::json!([reference]),
            "finding" => {
                value["investigation"]["findings"][0]["evidence"] = serde_json::json!([reference]);
            }
            _ => unreachable!(),
        }
        assert_eq!(
            validate_value(&value, &expectation).unwrap_err(),
            ReportRejection::BoundsExceeded,
            "{location} evidence must count UTF-8 bytes"
        );
    }
}

#[test]
fn enforces_inclusive_byte_and_collection_bounds() {
    let expectation = investigation_expectation();
    let mut boundary_text = json_value();
    boundary_text["summary"] = "é".repeat(MAX_TEXT_BYTES / 2).into();
    assert!(validate_value(&boundary_text, &expectation).is_ok());
    boundary_text["summary"] = format!("{}a", "é".repeat(MAX_TEXT_BYTES / 2)).into();
    assert_eq!(
        validate_value(&boundary_text, &expectation).unwrap_err(),
        ReportRejection::BoundsExceeded
    );

    let mut boundary_array = json_value();
    boundary_array["investigation"]["scope"] =
        serde_json::Value::Array(vec!["repo:file".into(); MAX_REPORT_ITEMS]);
    assert!(validate_value(&boundary_array, &expectation).is_ok());
    boundary_array["investigation"]["scope"] =
        serde_json::Value::Array(vec!["repo:file".into(); MAX_REPORT_ITEMS + 1]);
    assert_eq!(
        validate_value(&boundary_array, &expectation).unwrap_err(),
        ReportRejection::BoundsExceeded
    );

    let mut evidence_at_limit = json_value();
    let finding_evidence = [128, 128, 128, 127]
        .into_iter()
        .map(|count| serde_json::Value::Array(vec!["repo:e".into(); count]))
        .collect::<Vec<_>>();
    evidence_at_limit["investigation"]["findings"] = serde_json::Value::Array(
        finding_evidence
            .into_iter()
            .map(|evidence| {
                serde_json::json!({
                    "summary": "Bounded finding.",
                    "criterion_ids": ["AC-1"],
                    "evidence": evidence
                })
            })
            .collect(),
    );
    assert!(validate_value(&evidence_at_limit, &expectation).is_ok());
    assert_eq!(MAX_EVIDENCE_REFERENCES, 512);
    let mut evidence_over_limit = json_value();
    evidence_over_limit["investigation"]["findings"] = serde_json::Value::Array(
        [128; 5]
            .into_iter()
            .map(|count| {
                let evidence = serde_json::Value::Array(vec!["repo:e".into(); count]);
                serde_json::json!({
                    "summary": "Bounded finding.",
                    "criterion_ids": ["AC-1"],
                    "evidence": evidence
                })
            })
            .collect(),
    );
    assert_eq!(
        validate_value(&evidence_over_limit, &expectation).unwrap_err(),
        ReportRejection::BoundsExceeded,
        "the overall report evidence cap applies across all findings"
    );
    evidence_at_limit["investigation"]["findings"][0]["evidence"] =
        serde_json::Value::Array(vec!["repo:e".into(); 129]);
    assert_eq!(
        validate_value(&evidence_at_limit, &expectation).unwrap_err(),
        ReportRejection::BoundsExceeded
    );

    let exact_limit = format!(
        "{}{}",
        VALID_INVESTIGATION,
        " ".repeat(MAX_REPORT_BYTES - VALID_INVESTIGATION.len())
    );
    assert_eq!(exact_limit.len(), MAX_REPORT_BYTES);
    assert!(validate_report(exact_limit.as_bytes(), &expectation).is_ok());
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().canonicalize().unwrap();
        write_expected_report(&workspace, exact_limit.as_bytes());
        let exact_file = read_report(&workspace, &expectation);
        assert!(exact_file.rejection.is_none());
        assert_eq!(exact_file.raw_bytes.len(), MAX_REPORT_BYTES);
    }
    assert_eq!(
        validate_report(format!("{exact_limit} ").as_bytes(), &expectation).unwrap_err(),
        ReportRejection::Oversized
    );
}

#[test]
fn accepts_256_byte_utf8_identifiers_and_rejects_longer_or_unsafe_path_tokens() {
    let id = "é".repeat(MAX_IDENTIFIER_BYTES / 2);
    let expectation = ReportExpectation::new(
        id.clone(),
        "attempt-1",
        TaskKind::Investigation,
        BTreeMap::from([(id.clone(), true)]),
        BTreeMap::from([("V-1".to_owned(), false)]),
    )
    .unwrap();
    let mut value = json_value();
    value["run_id"] = id.clone().into();
    value["acceptance"][0]["criterion_id"] = id.clone().into();
    value["investigation"]["findings"][0]["criterion_ids"] = serde_json::json!([id]);
    validate_value(&value, &expectation).unwrap();
    let mut oversized_report_id = json_value();
    oversized_report_id["run_id"] = "r".repeat(MAX_IDENTIFIER_BYTES + 1).into();
    assert_eq!(
        validate_value(&oversized_report_id, &expectation).unwrap_err(),
        ReportRejection::BoundsExceeded
    );

    assert_eq!(
        ReportExpectation::new(
            "é".repeat((MAX_IDENTIFIER_BYTES / 2) + 1),
            "attempt-1",
            TaskKind::Investigation,
            BTreeMap::new(),
            BTreeMap::new()
        )
        .unwrap_err(),
        ReportRejection::InvalidExpectation
    );
    assert_eq!(
        ReportExpectation::new(
            "run-1",
            "../outside",
            TaskKind::Investigation,
            BTreeMap::new(),
            BTreeMap::new()
        )
        .unwrap_err(),
        ReportRejection::InvalidExpectation
    );
    for unsafe_token in ["..", ".", "../attempt-1", "attempt/1", "attempt\\1"] {
        assert_eq!(
            report_relative_path(unsafe_token),
            Err(ReportRejection::InvalidAttemptId)
        );
    }
    assert_eq!(
        report_relative_path(&"a".repeat(MAX_IDENTIFIER_BYTES)).unwrap(),
        Path::new(".coding-agent")
            .join("a".repeat(MAX_IDENTIFIER_BYTES))
            .join("result.json")
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn preserves_bounded_bytes_for_invalid_and_oversized_reports() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    write_expected_report(&workspace, b"{broken report");
    let invalid = read_report(&workspace, &investigation_expectation());
    assert_eq!(invalid.rejection, Some(ReportRejection::InvalidJson));
    assert_eq!(invalid.raw_bytes, b"{broken report");
    assert!(invalid.bytes_complete);

    write_expected_report(&workspace, &vec![b'x'; MAX_REPORT_BYTES + 91]);
    let oversized = read_report(&workspace, &investigation_expectation());
    assert_eq!(oversized.rejection, Some(ReportRejection::Oversized));
    assert_eq!(oversized.raw_bytes.len(), MAX_REPORT_BYTES);
    assert_eq!(oversized.observed_byte_count, MAX_REPORT_BYTES + 1);
    assert!(!oversized.bytes_complete);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn fixed_reader_rejects_missing_and_symlinked_workspace_report_paths() {
    let missing_root = tempfile::tempdir().unwrap();
    let missing_workspace = missing_root.path().canonicalize().unwrap();
    let missing = read_report(&missing_workspace, &investigation_expectation());
    assert_eq!(missing.rejection, Some(ReportRejection::MissingReport));

    let outside = tempfile::tempdir().unwrap();
    let outside_workspace = outside.path().canonicalize().unwrap();
    write_expected_report(&outside_workspace, VALID_INVESTIGATION.as_bytes());

    let parent_root = tempfile::tempdir().unwrap();
    let parent_workspace = parent_root.path().canonicalize().unwrap();
    std::os::unix::fs::symlink(
        outside_workspace.join(".coding-agent"),
        parent_workspace.join(".coding-agent"),
    )
    .unwrap();
    assert_eq!(
        read_report(&parent_workspace, &investigation_expectation()).rejection,
        Some(ReportRejection::UnsafeReportDirectory)
    );

    let attempt_root = tempfile::tempdir().unwrap();
    let attempt_workspace = attempt_root.path().canonicalize().unwrap();
    std::fs::create_dir_all(attempt_workspace.join(".coding-agent")).unwrap();
    std::os::unix::fs::symlink(
        outside_workspace.join(".coding-agent/attempt-1"),
        attempt_workspace.join(".coding-agent/attempt-1"),
    )
    .unwrap();
    assert_eq!(
        read_report(&attempt_workspace, &investigation_expectation()).rejection,
        Some(ReportRejection::UnsafeReportDirectory)
    );

    let file_root = tempfile::tempdir().unwrap();
    let file_workspace = file_root.path().canonicalize().unwrap();
    let report_path = file_workspace.join(report_relative_path("attempt-1").unwrap());
    std::fs::create_dir_all(report_path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(
        outside_workspace.join(".coding-agent/attempt-1/result.json"),
        report_path,
    )
    .unwrap();
    assert_eq!(
        read_report(&file_workspace, &investigation_expectation()).rejection,
        Some(ReportRejection::UnsafeReportFile)
    );

    let root_link_parent = tempfile::tempdir().unwrap();
    let root_link = root_link_parent.path().join("workspace-link");
    std::os::unix::fs::symlink(&outside_workspace, &root_link).unwrap();
    assert_eq!(
        read_report(&root_link, &investigation_expectation()).rejection,
        Some(ReportRejection::UnsafeWorkspaceRoot)
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn rejects_fifo_before_reading_so_an_agent_cannot_hang_the_host_reader() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let report_path = workspace.join(report_relative_path("attempt-1").unwrap());
    std::fs::create_dir_all(report_path.parent().unwrap()).unwrap();
    let path = std::ffi::CString::new(report_path.as_os_str().as_encoded_bytes()).unwrap();
    let result = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
    assert_eq!(result, 0);

    assert_eq!(
        read_report(&workspace, &investigation_expectation()).rejection,
        Some(ReportRejection::UnsafeReportFile)
    );
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_hosts_fail_closed_instead_of_using_path_based_file_reads() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let result = read_report(&workspace, &investigation_expectation());
    assert_eq!(result.rejection, Some(ReportRejection::UnsupportedPlatform));
}
