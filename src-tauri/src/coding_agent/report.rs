use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const MAX_REPORT_BYTES: usize = 1024 * 1024;
pub const MAX_REPORT_ITEMS: usize = 128;
pub const MAX_EVIDENCE_REFERENCES: usize = 512;
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_IDENTIFIER_BYTES: usize = 256;

/// Task identity and immutable acceptance/validation IDs supplied by the host.
/// The booleans are `required`; none of these values comes from the agent report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportExpectation {
    run_id: String,
    attempt_id: String,
    task_kind: TaskKind,
    acceptance_criteria: BTreeMap<String, bool>,
    validation_plan: BTreeMap<String, bool>,
}

impl ReportExpectation {
    pub fn new(
        run_id: impl Into<String>,
        attempt_id: impl Into<String>,
        task_kind: TaskKind,
        acceptance_criteria: BTreeMap<String, bool>,
        validation_plan: BTreeMap<String, bool>,
    ) -> Result<Self, ReportRejection> {
        let expectation = Self {
            run_id: run_id.into(),
            attempt_id: attempt_id.into(),
            task_kind,
            acceptance_criteria,
            validation_plan,
        };
        validate_expectation(&expectation)?;
        Ok(expectation)
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    pub fn task_kind(&self) -> TaskKind {
        self.task_kind
    }

    pub fn acceptance_criteria(&self) -> &BTreeMap<String, bool> {
        &self.acceptance_criteria
    }

    pub fn validation_plan(&self) -> &BTreeMap<String, bool> {
        &self.validation_plan
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Investigation,
    Implementation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentReportStatus {
    Completed,
    Partial,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceAssessment {
    Met,
    Unmet,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationClaim {
    Passed,
    Failed,
    NotRun,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReport {
    pub schema_version: u32,
    pub run_id: String,
    pub attempt_id: String,
    pub task_kind: TaskKind,
    pub status: AgentReportStatus,
    pub summary: String,
    pub acceptance: Vec<CriterionAssessment>,
    pub validation: Vec<ValidationReport>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub investigation: Option<InvestigationReport>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub implementation: Option<ImplementationReport>,
    pub unresolved: Vec<UnresolvedItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CriterionAssessment {
    pub criterion_id: String,
    pub assessment: AcceptanceAssessment,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationReport {
    pub validation_id: String,
    pub claimed_result: ValidationClaim,
    pub evidence: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvestigationReport {
    pub scope: Vec<String>,
    pub findings: Vec<InvestigationFinding>,
    pub limitations: Vec<String>,
    pub response_questions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvestigationFinding {
    pub summary: String,
    pub criterion_ids: Vec<String>,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImplementationReport {
    pub change_summary: String,
    pub changed_files: Vec<String>,
    pub implementation_notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnresolvedItem {
    pub description: String,
    pub blocks_completion: bool,
    pub criterion_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionAssessment {
    Completed,
    Partial,
    Blocked,
    Inconsistent,
    IncompleteRequiredValidation,
}

/// This assesses untrusted report claims, not host completion, exit, or validation proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportAssessment {
    /// Required validation claims must be confirmed independently before DONE.
    pub completion: CompletionAssessment,
    pub required_acceptance_not_met_or_unknown: Vec<String>,
    pub blocking_unresolved_items: Vec<usize>,
    /// Required checks for which the agent did not claim `passed`; these claims
    /// still require independent host validation before a DONE decision.
    pub required_validation_claims_not_passed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedReport {
    pub report: AgentReport,
    pub assessment: ReportAssessment,
}

/// The digest covers exactly `raw_bytes`; oversized reports retain only a prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportReadResult {
    pub validated_report: Option<ValidatedReport>,
    pub rejection: Option<ReportRejection>,
    pub raw_bytes: Vec<u8>,
    pub observed_byte_count: usize,
    pub reported_file_size: Option<u64>,
    pub raw_bytes_sha256: String,
    /// Reached EOF within the byte limit; this does not prove writers stopped.
    pub bytes_complete: bool,
}

impl ReportReadResult {
    fn rejected(rejection: ReportRejection) -> Self {
        Self::from_observed(Vec::new(), 0, None, false, None, Some(rejection))
    }

    fn from_observed(
        raw_bytes: Vec<u8>,
        observed_byte_count: usize,
        reported_file_size: Option<u64>,
        bytes_complete: bool,
        validated_report: Option<ValidatedReport>,
        rejection: Option<ReportRejection>,
    ) -> Self {
        let raw_bytes_sha256 = sha256(&raw_bytes);
        Self {
            validated_report,
            rejection,
            raw_bytes,
            observed_byte_count,
            reported_file_size,
            raw_bytes_sha256,
            bytes_complete,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ReportRejection {
    #[error("invalid report expectation")]
    InvalidExpectation,
    #[error("attempt id is not a safe report path component")]
    InvalidAttemptId,
    #[error("workspace root must be an existing canonical directory")]
    UnsafeWorkspaceRoot,
    #[error("secure report reading is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("report was not found")]
    MissingReport,
    #[error("report directory is not a real directory")]
    UnsafeReportDirectory,
    #[error("report is not a regular non-symlink file")]
    UnsafeReportFile,
    #[error("report exceeds the byte limit")]
    Oversized,
    #[error("report could not be read completely")]
    ReadFailed,
    #[error("report bytes are not UTF-8")]
    InvalidUtf8,
    #[error("report is not valid schema-v1 JSON")]
    InvalidJson,
    #[error("unsupported report schema version")]
    UnsupportedSchemaVersion,
    #[error("report run id does not match the approval snapshot")]
    RunIdMismatch,
    #[error("report attempt id does not match the selected attempt")]
    AttemptIdMismatch,
    #[error("report task kind does not match the approval snapshot")]
    TaskKindMismatch,
    #[error("acceptance IDs do not exactly match the approval snapshot")]
    AcceptanceIdMismatch,
    #[error("validation IDs do not exactly match the approved plan")]
    ValidationIdMismatch,
    #[error("report references an unknown acceptance criterion")]
    InvalidCriterionReference,
    #[error("report field or collection exceeds its size limit")]
    BoundsExceeded,
}

/// Return the only relative report location accepted for an attempt.
pub fn report_relative_path(attempt_id: &str) -> Result<PathBuf, ReportRejection> {
    if !is_safe_attempt_component(attempt_id) {
        return Err(ReportRejection::InvalidAttemptId);
    }
    Ok(PathBuf::from(".coding-agent")
        .join(attempt_id)
        .join("result.json"))
}

/// Parse, type-check, bound, and compare a report against the host snapshot.
pub fn validate_report(
    raw_bytes: &[u8],
    expectation: &ReportExpectation,
) -> Result<ValidatedReport, ReportRejection> {
    validate_expectation(expectation)?;
    if raw_bytes.len() > MAX_REPORT_BYTES {
        return Err(ReportRejection::Oversized);
    }
    std::str::from_utf8(raw_bytes).map_err(|_| ReportRejection::InvalidUtf8)?;
    let report: AgentReport =
        serde_json::from_slice(raw_bytes).map_err(|_| ReportRejection::InvalidJson)?;
    if report.schema_version != 1 {
        return Err(ReportRejection::UnsupportedSchemaVersion);
    }
    validate_report_bounds(&report)?;
    if report.run_id != expectation.run_id {
        return Err(ReportRejection::RunIdMismatch);
    }
    if report.attempt_id != expectation.attempt_id {
        return Err(ReportRejection::AttemptIdMismatch);
    }
    if report.task_kind != expectation.task_kind {
        return Err(ReportRejection::TaskKindMismatch);
    }

    validate_id_set(
        report
            .acceptance
            .iter()
            .map(|entry| entry.criterion_id.as_str()),
        expectation.acceptance_criteria.keys().map(String::as_str),
    )
    .map_err(|_| ReportRejection::AcceptanceIdMismatch)?;
    validate_id_set(
        report
            .validation
            .iter()
            .map(|entry| entry.validation_id.as_str()),
        expectation.validation_plan.keys().map(String::as_str),
    )
    .map_err(|_| ReportRejection::ValidationIdMismatch)?;

    match expectation.task_kind {
        TaskKind::Investigation
            if report.investigation.is_none() || report.implementation.is_some() =>
        {
            return Err(ReportRejection::TaskKindMismatch);
        }
        TaskKind::Implementation
            if report.implementation.is_none() || report.investigation.is_some() =>
        {
            return Err(ReportRejection::TaskKindMismatch);
        }
        _ => {}
    }

    let criterion_ids = expectation
        .acceptance_criteria
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if report
        .investigation
        .as_ref()
        .into_iter()
        .flat_map(|investigation| investigation.findings.iter())
        .flat_map(|finding| finding.criterion_ids.iter())
        .chain(
            report
                .unresolved
                .iter()
                .flat_map(|item| item.criterion_ids.iter()),
        )
        .any(|id| !criterion_ids.contains(id.as_str()))
    {
        return Err(ReportRejection::InvalidCriterionReference);
    }

    let assessment = assess_report(&report, expectation);
    Ok(ValidatedReport { report, assessment })
}

/// Read only `.coding-agent/<attempt>/result.json` relative to the canonical
/// workspace. No report-supplied path is ever opened.
pub fn read_report(workspace_root: &Path, expectation: &ReportExpectation) -> ReportReadResult {
    if let Err(rejection) = validate_expectation(expectation) {
        return ReportReadResult::rejected(rejection);
    }
    if report_relative_path(&expectation.attempt_id).is_err() {
        return ReportReadResult::rejected(ReportRejection::InvalidAttemptId);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        read_report_unix(workspace_root, expectation)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = workspace_root;
        ReportReadResult::rejected(ReportRejection::UnsupportedPlatform)
    }
}

fn validate_expectation(expectation: &ReportExpectation) -> Result<(), ReportRejection> {
    if expectation.run_id.is_empty()
        || expectation.run_id.len() > MAX_IDENTIFIER_BYTES
        || expectation.attempt_id.is_empty()
        || expectation.attempt_id.len() > MAX_IDENTIFIER_BYTES
        || !is_safe_attempt_component(&expectation.attempt_id)
        || expectation.acceptance_criteria.len() > MAX_REPORT_ITEMS
        || expectation.validation_plan.len() > MAX_REPORT_ITEMS
        || expectation
            .acceptance_criteria
            .keys()
            .chain(expectation.validation_plan.keys())
            .any(|id| !is_valid_identifier(id))
    {
        return Err(ReportRejection::InvalidExpectation);
    }
    Ok(())
}

fn is_valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES
}

fn is_safe_attempt_component(value: &str) -> bool {
    is_valid_identifier(value)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_id_set<'a>(
    actual: impl Iterator<Item = &'a str>,
    expected: impl Iterator<Item = &'a str>,
) -> Result<(), ()> {
    let mut actual_set = BTreeSet::new();
    let mut actual_len = 0;
    for id in actual {
        actual_len += 1;
        if !actual_set.insert(id) {
            return Err(());
        }
    }
    let expected_set = expected.collect::<BTreeSet<_>>();
    if actual_len != expected_set.len() || actual_set != expected_set {
        return Err(());
    }
    Ok(())
}

fn validate_report_bounds(report: &AgentReport) -> Result<(), ReportRejection> {
    let mut evidence_count = 0usize;
    check_text(&report.run_id, true)?;
    check_text(&report.attempt_id, true)?;
    check_text(&report.summary, false)?;
    check_item_count(report.acceptance.len())?;
    check_item_count(report.validation.len())?;
    check_item_count(report.unresolved.len())?;

    for entry in &report.acceptance {
        check_text(&entry.criterion_id, true)?;
        evidence_count = evidence_count.saturating_add(check_text_array(&entry.evidence, false)?);
    }
    for entry in &report.validation {
        check_text(&entry.validation_id, true)?;
        check_text(&entry.reason, false)?;
        evidence_count = evidence_count.saturating_add(check_text_array(&entry.evidence, false)?);
    }
    if let Some(investigation) = &report.investigation {
        check_text_array(&investigation.scope, false)?;
        check_item_count(investigation.findings.len())?;
        check_text_array(&investigation.limitations, false)?;
        check_text_array(&investigation.response_questions, false)?;
        for finding in &investigation.findings {
            check_text(&finding.summary, false)?;
            check_text_array(&finding.criterion_ids, true)?;
            evidence_count =
                evidence_count.saturating_add(check_text_array(&finding.evidence, false)?);
        }
    }
    if let Some(implementation) = &report.implementation {
        check_text(&implementation.change_summary, false)?;
        check_text_array(&implementation.changed_files, false)?;
        check_text_array(&implementation.implementation_notes, false)?;
    }
    for item in &report.unresolved {
        check_text(&item.description, false)?;
        check_text_array(&item.criterion_ids, true)?;
    }
    if evidence_count > MAX_EVIDENCE_REFERENCES {
        return Err(ReportRejection::BoundsExceeded);
    }
    Ok(())
}

fn check_item_count(length: usize) -> Result<(), ReportRejection> {
    (length <= MAX_REPORT_ITEMS)
        .then_some(())
        .ok_or(ReportRejection::BoundsExceeded)
}

fn check_text_array(values: &[String], identifiers: bool) -> Result<usize, ReportRejection> {
    check_item_count(values.len())?;
    for value in values {
        check_text(value, identifiers)?;
    }
    Ok(values.len())
}

fn check_text(value: &str, identifier: bool) -> Result<(), ReportRejection> {
    let maximum = if identifier {
        MAX_IDENTIFIER_BYTES
    } else {
        MAX_TEXT_BYTES
    };
    (value.len() <= maximum && (!identifier || !value.is_empty()))
        .then_some(())
        .ok_or(ReportRejection::BoundsExceeded)
}

fn assess_report(report: &AgentReport, expectation: &ReportExpectation) -> ReportAssessment {
    let acceptance_by_id = report
        .acceptance
        .iter()
        .map(|entry| (entry.criterion_id.as_str(), entry.assessment))
        .collect::<BTreeMap<_, _>>();
    let required_acceptance_not_met_or_unknown = expectation
        .acceptance_criteria
        .iter()
        .filter(|(id, required)| {
            **required && acceptance_by_id.get(id.as_str()) != Some(&AcceptanceAssessment::Met)
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let blocking_unresolved_items = report
        .unresolved
        .iter()
        .enumerate()
        .filter_map(|(index, item)| item.blocks_completion.then_some(index))
        .collect::<Vec<_>>();
    let validations_by_id = report
        .validation
        .iter()
        .map(|entry| (entry.validation_id.as_str(), entry.claimed_result))
        .collect::<BTreeMap<_, _>>();
    let required_validation_claims_not_passed = expectation
        .validation_plan
        .iter()
        .filter(|(id, required)| {
            **required && validations_by_id.get(id.as_str()) != Some(&ValidationClaim::Passed)
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();

    let completion = match report.status {
        AgentReportStatus::Completed
            if !required_acceptance_not_met_or_unknown.is_empty()
                || !blocking_unresolved_items.is_empty() =>
        {
            CompletionAssessment::Inconsistent
        }
        AgentReportStatus::Completed if !required_validation_claims_not_passed.is_empty() => {
            CompletionAssessment::IncompleteRequiredValidation
        }
        AgentReportStatus::Completed => CompletionAssessment::Completed,
        AgentReportStatus::Partial => CompletionAssessment::Partial,
        AgentReportStatus::Blocked => CompletionAssessment::Blocked,
    };
    ReportAssessment {
        completion,
        required_acceptance_not_met_or_unknown,
        blocking_unresolved_items,
        required_validation_claims_not_passed,
    }
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_report_unix(workspace_root: &Path, expectation: &ReportExpectation) -> ReportReadResult {
    use std::{
        ffi::CString,
        fs,
        os::{
            fd::FromRawFd,
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
    };

    let Ok(canonical_root) = workspace_root.canonicalize() else {
        return ReportReadResult::rejected(ReportRejection::UnsafeWorkspaceRoot);
    };
    let Ok(root_metadata) = fs::symlink_metadata(workspace_root) else {
        return ReportReadResult::rejected(ReportRejection::UnsafeWorkspaceRoot);
    };
    if !workspace_root.is_absolute()
        || canonical_root != workspace_root
        || root_metadata.file_type().is_symlink()
        || !root_metadata.is_dir()
    {
        return ReportReadResult::rejected(ReportRejection::UnsafeWorkspaceRoot);
    }
    let Ok(root_name) = CString::new(workspace_root.as_os_str().as_bytes()) else {
        return ReportReadResult::rejected(ReportRejection::UnsafeWorkspaceRoot);
    };
    // The root is pinned once; every untrusted descendant is opened relative to it.
    let root_fd = unsafe {
        libc::open(
            root_name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if root_fd < 0 {
        return ReportReadResult::rejected(ReportRejection::UnsafeWorkspaceRoot);
    }
    let root = unsafe { std::fs::File::from_raw_fd(root_fd) };
    let mut opened_root = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(root.as_raw_fd(), &mut opened_root) } != 0
        || opened_root.st_dev as u64 != root_metadata.dev()
        || opened_root.st_ino as u64 != root_metadata.ino()
    {
        return ReportReadResult::rejected(ReportRejection::UnsafeWorkspaceRoot);
    }

    let Ok(agent_directory) = CString::new(".coding-agent") else {
        unreachable!("constant contains no NUL")
    };
    let agent_directory = match open_directory_at(root.as_raw_fd(), &agent_directory) {
        Ok(directory) => directory,
        Err(error) => {
            return ReportReadResult::rejected(classify_directory_open_error(&error));
        }
    };
    let Ok(attempt_directory) = CString::new(expectation.attempt_id.as_bytes()) else {
        return ReportReadResult::rejected(ReportRejection::InvalidAttemptId);
    };
    let attempt_directory = match open_directory_at(agent_directory.as_raw_fd(), &attempt_directory)
    {
        Ok(directory) => directory,
        Err(error) => {
            return ReportReadResult::rejected(classify_directory_open_error(&error));
        }
    };

    let Ok(result_name) = CString::new("result.json") else {
        unreachable!("constant contains no NUL")
    };
    let mut path_stat = unsafe { std::mem::zeroed::<libc::stat>() };
    let stat_result = unsafe {
        libc::fstatat(
            attempt_directory.as_raw_fd(),
            result_name.as_ptr(),
            &mut path_stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if stat_result != 0 {
        let error = std::io::Error::last_os_error();
        return ReportReadResult::rejected(classify_file_open_error(&error));
    }
    if path_stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return ReportReadResult::rejected(ReportRejection::UnsafeReportFile);
    }

    // O_NONBLOCK prevents a raced FIFO replacement from hanging before fstat.
    let report_fd = unsafe {
        libc::openat(
            attempt_directory.as_raw_fd(),
            result_name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if report_fd < 0 {
        let error = std::io::Error::last_os_error();
        return ReportReadResult::rejected(classify_file_open_error(&error));
    }
    let report_file = unsafe { std::fs::File::from_raw_fd(report_fd) };
    let mut opened_file = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(report_file.as_raw_fd(), &mut opened_file) } != 0
        || opened_file.st_mode & libc::S_IFMT != libc::S_IFREG
        || opened_file.st_dev != path_stat.st_dev
        || opened_file.st_ino != path_stat.st_ino
    {
        return ReportReadResult::rejected(ReportRejection::UnsafeReportFile);
    }

    let reported_file_size = u64::try_from(opened_file.st_size).ok();
    let mut captured = Vec::with_capacity(
        reported_file_size
            .unwrap_or_default()
            .min(MAX_REPORT_BYTES as u64) as usize,
    );
    let read_result = report_file
        .take((MAX_REPORT_BYTES as u64) + 1)
        .read_to_end(&mut captured);
    let observed_byte_count = captured.len();
    let read_failed = read_result.is_err();
    let oversized = observed_byte_count > MAX_REPORT_BYTES
        || reported_file_size.is_some_and(|size| size > MAX_REPORT_BYTES as u64);
    let size_changed =
        reported_file_size.is_some_and(|size| size != observed_byte_count as u64 && !oversized);
    captured.truncate(MAX_REPORT_BYTES);
    let bytes_complete = !read_failed && !oversized && !size_changed;
    if oversized {
        return ReportReadResult::from_observed(
            captured,
            observed_byte_count,
            reported_file_size,
            false,
            None,
            Some(ReportRejection::Oversized),
        );
    }
    if read_failed || size_changed {
        return ReportReadResult::from_observed(
            captured,
            observed_byte_count,
            reported_file_size,
            false,
            None,
            Some(ReportRejection::ReadFailed),
        );
    }

    match validate_report(&captured, expectation) {
        Ok(validated_report) => ReportReadResult::from_observed(
            captured,
            observed_byte_count,
            reported_file_size,
            bytes_complete,
            Some(validated_report),
            None,
        ),
        Err(rejection) => ReportReadResult::from_observed(
            captured,
            observed_byte_count,
            reported_file_size,
            bytes_complete,
            None,
            Some(rejection),
        ),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_directory_at(
    parent_fd: std::os::fd::RawFd,
    name: &std::ffi::CStr,
) -> Result<std::fs::File, std::io::Error> {
    use std::os::fd::FromRawFd;

    let fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn classify_directory_open_error(error: &std::io::Error) -> ReportRejection {
    match error.raw_os_error() {
        Some(libc::ENOENT) => ReportRejection::MissingReport,
        _ => ReportRejection::UnsafeReportDirectory,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn classify_file_open_error(error: &std::io::Error) -> ReportRejection {
    match error.raw_os_error() {
        Some(libc::ENOENT) => ReportRejection::MissingReport,
        _ => ReportRejection::UnsafeReportFile,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::AsRawFd;
