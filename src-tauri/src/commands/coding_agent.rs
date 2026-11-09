//! Tauri boundary for the app-owned coding-agent coordinator.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager, State, WebviewWindow};

use crate::{
    coding_agent::{
        AgentRunRecord, CompletionAuthority, CompletionOutcome, CompletionPolicy,
        CompletionTracker, ExecutionMode, HumanCorrectionRequest, HumanReviewApproval,
        IssueState as CompletionIssueState, PrimaryIssueTrackerAdapter, PublicationPlan,
        RepositoryIdentity, RepositoryResolution, RepositorySelectionReason,
        RunCompletionAuthorization, RunId, RunPurpose, RunState, TrackerError,
        TrackerIssueSnapshot, TrackerMutationRequest,
        assessment::{AllowedEvidenceSource, EvidenceSourceKind, ProviderSelection},
        completion::TrackerIssueOperationSnapshot,
        model::{
            ApprovalAssessmentCommand, ApprovalAssessmentContract, ApprovalCriterionPolicy,
            ApprovalValidationDraft, ApprovedJudgeInputField, AssessmentInputScope,
            CriterionEvaluationMethod, ValidationPermission,
        },
        settings::{
            CodingAgentSettings, NativeRepositoryApprovalSettings, RepositoryIdentitySettings,
            SettingsError,
        },
    },
    commands::AppState,
    error::{AppError, AppResult},
    native_terminal::registry::NativeSetupState,
};

pub use crate::coding_agent::repository_check::{
    CheckCode, CheckStageName, CheckStageStatus, RepositoryHostCheckRequest,
    RepositoryHostCheckResponse, RepositoryHostCheckStage,
};

const MAX_EVIDENCE_READ_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_APPROVAL_COMMAND_BYTES: usize = 64 * 1024;
const MAX_APPROVAL_TEXT_BYTES: usize = 16 * 1024;
const HUMAN_REVIEW_POLICY: &str = "agent-app/human-review";
const AUTO_COMPLETION_POLICY: &str = "lookback/coding-agent-auto-done";
const APPROVED_JUDGE_PROMPT_POLICY_VERSION: &str = "coding-agent-judge-v1";

struct ExactReviewTracker {
    inner: Arc<dyn CompletionTracker>,
    run_id: RunId,
    issue_id: String,
    expected_revision: u64,
    expected_marker: CodingAgentReviewMarker,
    target_state: CompletionIssueState,
    transition_started: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl CompletionTracker for ExactReviewTracker {
    async fn read_issue_with_history(
        &self,
        issue_id: &str,
    ) -> crate::coding_agent::TrackerResult<TrackerIssueSnapshot> {
        let snapshot = self.inner.read_issue_with_history(issue_id).await?;
        self.validate_snapshot(issue_id, &snapshot)?;
        Ok(snapshot)
    }

    async fn read_issue_with_operation_history(
        &self,
        issue_id: &str,
    ) -> crate::coding_agent::TrackerResult<TrackerIssueOperationSnapshot> {
        let snapshot = self
            .inner
            .read_issue_with_operation_history(issue_id)
            .await?;
        self.validate_snapshot(issue_id, &snapshot.issue)?;
        Ok(snapshot)
    }

    fn primary_identity(
        &self,
    ) -> crate::coding_agent::TrackerResult<crate::coding_agent::model::PrimaryTrackerIdentity>
    {
        self.inner.primary_identity()
    }

    async fn transition_issue(
        &self,
        request: TrackerMutationRequest,
    ) -> crate::coding_agent::TrackerResult<()> {
        self.validate_transition_request(&request)?;
        self.transition_started
            .store(true, std::sync::atomic::Ordering::Release);
        self.inner.transition_issue(request).await
    }

    async fn transition_issue_with_receipt(
        &self,
        request: TrackerMutationRequest,
    ) -> crate::coding_agent::TrackerResult<crate::coding_agent::model::PrimaryOperationReceipt>
    {
        self.validate_transition_request(&request)?;
        self.transition_started
            .store(true, std::sync::atomic::Ordering::Release);
        self.inner.transition_issue_with_receipt(request).await
    }

    async fn lookup_transition_operation(
        &self,
        authorization: &RunCompletionAuthorization,
    ) -> crate::coding_agent::completion::PrimaryOperationLookup {
        if authorization.run_id != self.run_id
            || authorization.issue_id != self.issue_id
            || authorization.target_issue_revision != self.expected_revision
        {
            return crate::coding_agent::completion::PrimaryOperationLookup::Unknown;
        }
        self.inner.lookup_transition_operation(authorization).await
    }
}

impl ExactReviewTracker {
    fn validate_snapshot(
        &self,
        issue_id: &str,
        snapshot: &TrackerIssueSnapshot,
    ) -> crate::coding_agent::TrackerResult<()> {
        let transition_started = self
            .transition_started
            .load(std::sync::atomic::Ordering::Acquire);
        if snapshot.issue_id != self.issue_id
            || issue_id != self.issue_id
            || !snapshot_has_exact_review_marker(snapshot, &self.expected_marker)
            || if transition_started {
                snapshot.revision < self.expected_revision
            } else {
                snapshot.revision != self.expected_revision
                    || snapshot.state != CompletionIssueState::AwaitingReview
            }
        {
            return Err(TrackerError::Unconfirmed);
        }
        Ok(())
    }

    fn validate_transition_request(
        &self,
        request: &TrackerMutationRequest,
    ) -> crate::coding_agent::TrackerResult<()> {
        if request.run_id != self.run_id
            || request.issue_id != self.issue_id
            || request.expected_revision != self.expected_revision
            || request.target_state != self.target_state
        {
            return Err(TrackerError::Rejected);
        }
        Ok(())
    }
}

fn snapshot_has_exact_review_marker(
    snapshot: &TrackerIssueSnapshot,
    expected: &CodingAgentReviewMarker,
) -> bool {
    let latest_marker = snapshot
        .history
        .iter()
        .filter(|entry| {
            entry.issue_id == snapshot.issue_id
                && entry.new_state == CompletionIssueState::AwaitingReview
        })
        .max_by_key(|entry| entry.resulting_revision)
        .and_then(|entry| entry.awaiting_review.as_ref());
    latest_marker.is_some_and(|marker| {
        marker.run_id == expected.run_id
            && marker.full_commit_id == expected.full_commit_id
            && marker.evidence_references == expected.evidence_references
    })
}

#[derive(Default)]
pub struct CodingAgentRunOwnerRegistry {
    owners: parking_lot::RwLock<HashMap<String, String>>,
}

impl CodingAgentRunOwnerRegistry {
    pub fn claim(&self, run_id: &str, window_label: &str) -> Result<(), &'static str> {
        let mut owners = self.owners.write();
        match owners.get(run_id) {
            Some(owner) if owner != window_label => Err("owned_by_another_window"),
            Some(_) => Ok(()),
            None => {
                owners.insert(run_id.to_owned(), window_label.to_owned());
                Ok(())
            }
        }
    }

    pub fn is_owned_by(&self, run_id: &str, window_label: &str) -> bool {
        self.owners
            .read()
            .get(run_id)
            .is_some_and(|owner| owner == window_label)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentSettingsResponse {
    pub settings: CodingAgentSettings,
    pub profile_mount_availability:
        Vec<crate::coding_agent::settings::CodingAgentProfileMountAvailability>,
    pub available_agent_presets: Vec<crate::coding_agent::presets::CodingAgentPreset>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentNativeSetupState {
    NotApproved,
    Approved,
    ReapprovalRequired,
    PackageMismatch,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentNativeSetupUnavailableReason {
    UnsupportedPlatform,
    PackageMissing,
    TrustedPolicyUnsafe,
    TrustedPolicyInvalid,
    StorageUnavailable,
    ExecutorUnavailable,
    SettingsUnavailable,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentNativeSetupPackageProvenance {
    Release,
    Development,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentNativeSetupApprovalSource {
    Gui,
    LegacyPolicy,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentNativeSetupResponse {
    pub settings_revision: u64,
    pub state: CodingAgentNativeSetupState,
    pub package_fingerprint: Option<String>,
    pub package_version: Option<String>,
    pub repository_approvals: Vec<NativeRepositoryApprovalSettings>,
    pub unavailable_reason: Option<CodingAgentNativeSetupUnavailableReason>,
    pub package_provenance: Option<CodingAgentNativeSetupPackageProvenance>,
    pub approval_source: CodingAgentNativeSetupApprovalSource,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAgentApproveNativeBackendRequest {
    pub expected_revision: u64,
    pub expected_package_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateCodingAgentSettingsRequest {
    pub expected_revision: u64,
    pub settings: CodingAgentSettings,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAgentRunPreviewRequest {
    pub issue_id: String,
    pub profile_id: String,
    pub execution_mode: ExecutionMode,
    pub purpose: RunPurpose,
    pub publication_plan: PublicationPlan,
    #[serde(default)]
    pub manual_repository: Option<RepositoryIdentitySettings>,
    #[serde(default)]
    pub auto_done_requested: bool,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub criterion_policies: Option<Vec<CodingAgentCriterionPolicyRequest>>,
    #[serde(default)]
    pub validation_drafts: Vec<CodingAgentValidationDraftRequest>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAgentCriterionPolicyRequest {
    pub criterion_id: String,
    pub required: bool,
    pub method: CriterionEvaluationMethod,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAgentValidationDraftRequest {
    pub required: bool,
    pub command: Option<CodingAgentValidationCommandRequest>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAgentValidationCommandRequest {
    pub argv: Vec<String>,
    pub working_directory: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentRunPreview {
    pub plan_id: String,
    pub digest: String,
    pub issue_id: String,
    pub issue_revision: u64,
    pub issue_title: String,
    pub issue_state: String,
    pub source_labels: Vec<String>,
    pub settings_revision: u64,
    pub profile_id: Option<String>,
    pub profile_version: Option<u64>,
    pub execution_mode: ExecutionMode,
    pub purpose: RunPurpose,
    pub publication_plan: PublicationPlan,
    pub selected_repository: Option<RepositoryIdentitySettings>,
    pub repository_selection_reason: Option<String>,
    pub repository_candidates: Vec<RepositoryIdentitySettings>,
    pub auto_done_requested: bool,
    pub assessment_contract: ApprovalAssessmentContract,
    pub assessment_contract_digest: String,
    pub assessment_provider_supported: bool,
    pub blockers: Vec<String>,
    pub can_approve: bool,
    pub base_commit: Option<String>,
    pub workspace_branch: Option<String>,
    pub image_digest: Option<String>,
    #[serde(skip)]
    pub preparation_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApproveAndStartCodingAgentRequest {
    #[serde(default)]
    pub delegation_plan_id: Option<String>,
    pub preview: CodingAgentRunPreviewRequest,
    pub expected_plan_id: String,
    pub expected_digest: String,
    pub user_confirmed: bool,
    pub manual_repository_confirmed: bool,
    pub auto_done_confirmed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentStartResponse {
    pub started: bool,
    pub run_id: Option<String>,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentRunStatus {
    pub run_id: String,
    pub issue_id: String,
    pub issue_revision: u64,
    pub revision: u64,
    pub execution_mode: ExecutionMode,
    pub purpose: RunPurpose,
    pub state: RunState,
    pub stage: crate::coding_agent::RunStage,
    pub profile_id: String,
    pub profile_version: u64,
    pub current_attempt_id: Option<String>,
    pub auto_done: bool,
    pub workspace_path: Option<String>,
    pub workspace_branch: String,
    pub commit_id: Option<String>,
    pub stdout_bytes: Option<u64>,
    pub stderr_bytes: Option<u64>,
    pub evidence: Vec<CodingAgentEvidenceReference>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentEvidenceReference {
    pub evidence_id: String,
    pub attempt_id: String,
    pub name: String,
    pub media_type: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub save_confirmed: bool,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentRunActionResponse {
    pub run: CodingAgentRunStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentRunOwnerResponse {
    pub run_id: String,
    pub window_label: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentEvidenceResponse {
    pub evidence_id: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentReviewAction {
    Complete,
    RequestChanges,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentReviewEvidenceKind {
    TestResult,
    Artifact,
    OtherEvidence,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAgentReviewEvidenceInput {
    pub kind: CodingAgentReviewEvidenceKind,
    pub summary: String,
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAgentReviewReportInput {
    pub evidence: Vec<CodingAgentReviewEvidenceInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAgentReviewMarker {
    pub run_id: String,
    pub full_commit_id: String,
    pub evidence_references: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentReviewSnapshot {
    pub semantic_reassessment_available: bool,
    pub run: CodingAgentRunStatus,
    pub issue_state: String,
    pub issue_revision: u64,
    pub marker: CodingAgentReviewMarker,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAgentReviewActionRequest {
    pub run_id: String,
    pub expected_run_revision: u64,
    pub expected_issue_revision: u64,
    pub expected_marker: CodingAgentReviewMarker,
    pub action: CodingAgentReviewAction,
    pub rationale: String,
    pub report: Option<CodingAgentReviewReportInput>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAgentReassessmentRequest {
    pub run_id: String,
    pub expected_run_revision: u64,
    pub expected_issue_revision: u64,
    pub expected_marker: CodingAgentReviewMarker,
    pub request_key: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentReviewActionResponse {
    pub outcome: String,
    pub run: Option<CodingAgentRunStatus>,
    pub issue_revision: u64,
}

#[tauri::command]
pub async fn coding_agent_get_settings(
    state: State<'_, AppState>,
) -> AppResult<CodingAgentSettingsResponse> {
    let store = Arc::clone(&state.coding_agent_settings);
    // Mount availability probes the filesystem per profile mount.
    run_blocking_work(move || {
        let settings = store.load().map_err(map_settings_error)?;
        let managed_home_root = store
            .managed_profile_home_root()
            .map_err(map_settings_error)?;
        Ok(settings_response(settings, &managed_home_root))
    })
    .await?
}

/// Run settings/registry work on the blocking pool. It does filesystem IO, and
/// native approval re-hashes the whole executor package (hundreds of MB); a
/// synchronous Tauri command would run that on the main thread and freeze the
/// webview.
async fn run_blocking_work<T, F>(work: F) -> AppResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| AppError::Config(format!("coding agent settings task failed: {error}")))
}

#[tauri::command]
pub async fn coding_agent_get_native_setup(
    state: State<'_, AppState>,
) -> AppResult<CodingAgentNativeSetupResponse> {
    let registry = Arc::clone(&state.native_terminal_registry);
    let snapshot = run_blocking_work(move || registry.native_setup()).await?;
    Ok(native_setup_response(snapshot))
}

#[tauri::command]
pub async fn coding_agent_approve_native_backend(
    state: State<'_, AppState>,
    request: CodingAgentApproveNativeBackendRequest,
) -> AppResult<CodingAgentNativeSetupResponse> {
    let _admission = settings_admission(&state).await?;
    let registry = Arc::clone(&state.native_terminal_registry);
    let snapshot = run_blocking_work(move || {
        registry.approve_native_backend(
            request.expected_revision,
            request.expected_package_fingerprint.as_deref(),
        )
    })
    .await?
    .map_err(|error| AppError::Config(error.user_message().to_owned()))?;
    Ok(native_setup_response(snapshot))
}

#[tauri::command]
pub async fn coding_agent_check_repository_host(
    state: State<'_, AppState>,
    request: RepositoryHostCheckRequest,
) -> AppResult<RepositoryHostCheckResponse> {
    state.ensure_coding_agent_accepting()?;
    state
        .native_terminal_registry
        .check_repository_host(request)
        .await
        .map_err(|error| AppError::Config(error.user_message().to_owned()))
}

#[tauri::command]
pub async fn coding_agent_update_settings(
    state: State<'_, AppState>,
    request: UpdateCodingAgentSettingsRequest,
) -> AppResult<CodingAgentSettingsResponse> {
    let _admission = settings_admission(&state).await?;
    let unresolved_profiles = unresolved_profile_ids(&state)?;
    let settings = state
        .coding_agent_settings
        .compare_and_swap(
            request.expected_revision,
            request.settings,
            &unresolved_profiles,
        )
        .map_err(map_settings_error)?;
    let managed_home_root = state
        .coding_agent_settings
        .managed_profile_home_root()
        .map_err(map_settings_error)?;
    Ok(settings_response(settings, &managed_home_root))
}

pub(crate) fn unresolved_profile_ids(state: &AppState) -> AppResult<HashSet<String>> {
    let mut unresolved_profiles = state
        .coding_agent_coordinator
        .recoverable_runs()
        .map_err(|_| AppError::Config("coding-agent run state is unavailable".into()))?
        .into_iter()
        .map(|run| run.snapshot.profile.profile_id)
        .collect::<HashSet<_>>();
    unresolved_profiles.extend(
        state
            .native_terminal_registry
            .unresolved_profile_ids()
            .map_err(|_| AppError::Config("native session state is unavailable".into()))?,
    );
    Ok(unresolved_profiles)
}

pub(crate) async fn settings_admission(
    state: &AppState,
) -> AppResult<tokio::sync::MutexGuard<'_, ()>> {
    state.ensure_coding_agent_accepting()?;
    let admission = state.native_terminal_admission.lock().await;
    state.ensure_coding_agent_accepting()?;
    Ok(admission)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrepareCodingAgentProfileHomeRequest {
    pub expected_revision: u64,
    pub profile_id: String,
}

#[tauri::command]
pub async fn coding_agent_prepare_profile_home(
    state: State<'_, AppState>,
    request: PrepareCodingAgentProfileHomeRequest,
) -> AppResult<CodingAgentSettingsResponse> {
    let _admission = settings_admission(&state).await?;
    let unresolved = unresolved_profile_ids(&state)?;
    let settings = state
        .coding_agent_settings
        .prepare_profile_home(request.expected_revision, &request.profile_id, &unresolved)
        .map_err(map_settings_error)?;
    let managed_home_root = state
        .coding_agent_settings
        .managed_profile_home_root()
        .map_err(map_settings_error)?;
    Ok(settings_response(settings, &managed_home_root))
}

fn settings_response(
    settings: CodingAgentSettings,
    managed_home_root: &Path,
) -> CodingAgentSettingsResponse {
    let profile_mount_availability =
        settings.profile_mount_availability_with_managed_home_root(managed_home_root);
    CodingAgentSettingsResponse {
        settings,
        profile_mount_availability,
        available_agent_presets: crate::coding_agent::presets::available_agent_presets(),
    }
}

fn native_setup_response(
    snapshot: crate::native_terminal::registry::NativeSetupSnapshot,
) -> CodingAgentNativeSetupResponse {
    use crate::native_terminal::registry::{
        NativeSetupApprovalSource, NativeSetupPackageProvenance, NativeSetupUnavailableReason,
    };
    CodingAgentNativeSetupResponse {
        settings_revision: snapshot.settings_revision,
        state: match snapshot.state {
            NativeSetupState::NotApproved => CodingAgentNativeSetupState::NotApproved,
            NativeSetupState::Approved => CodingAgentNativeSetupState::Approved,
            NativeSetupState::ReapprovalRequired => CodingAgentNativeSetupState::ReapprovalRequired,
            NativeSetupState::PackageMismatch => CodingAgentNativeSetupState::PackageMismatch,
            NativeSetupState::Unavailable => CodingAgentNativeSetupState::Unavailable,
        },
        package_fingerprint: snapshot.package_fingerprint,
        package_version: snapshot.package_version,
        repository_approvals: snapshot.repository_approvals,
        unavailable_reason: snapshot.unavailable_reason.map(|reason| match reason {
            NativeSetupUnavailableReason::UnsupportedPlatform => {
                CodingAgentNativeSetupUnavailableReason::UnsupportedPlatform
            }
            NativeSetupUnavailableReason::PackageMissing => {
                CodingAgentNativeSetupUnavailableReason::PackageMissing
            }
            NativeSetupUnavailableReason::TrustedPolicyUnsafe => {
                CodingAgentNativeSetupUnavailableReason::TrustedPolicyUnsafe
            }
            NativeSetupUnavailableReason::TrustedPolicyInvalid => {
                CodingAgentNativeSetupUnavailableReason::TrustedPolicyInvalid
            }
            NativeSetupUnavailableReason::StorageUnavailable => {
                CodingAgentNativeSetupUnavailableReason::StorageUnavailable
            }
            NativeSetupUnavailableReason::ExecutorUnavailable => {
                CodingAgentNativeSetupUnavailableReason::ExecutorUnavailable
            }
            NativeSetupUnavailableReason::SettingsUnavailable => {
                CodingAgentNativeSetupUnavailableReason::SettingsUnavailable
            }
        }),
        package_provenance: snapshot
            .package_provenance
            .map(|provenance| match provenance {
                NativeSetupPackageProvenance::Release => {
                    CodingAgentNativeSetupPackageProvenance::Release
                }
                NativeSetupPackageProvenance::Development => {
                    CodingAgentNativeSetupPackageProvenance::Development
                }
            }),
        approval_source: match snapshot.approval_source {
            NativeSetupApprovalSource::Gui => CodingAgentNativeSetupApprovalSource::Gui,
            NativeSetupApprovalSource::LegacyPolicy => {
                CodingAgentNativeSetupApprovalSource::LegacyPolicy
            }
        },
    }
}

#[tauri::command]
pub async fn coding_agent_get_backend_status(
    state: State<'_, AppState>,
) -> AppResult<crate::coding_agent::backend::CodingAgentBackendStatus> {
    let mode = state.service_mode(crate::commands::connection::Service::Jobworkerp);
    let may_connect = mode == crate::commands::connection::ConnectionMode::Local
        && state.sidecars.current_endpoints().is_some();
    Ok(state
        .coding_agent_backend
        .status_with_jobworkerp_future(mode, async {
            if may_connect {
                state.jobworkerp().await.ok()
            } else {
                None
            }
        })
        .await)
}

#[tauri::command]
pub async fn coding_agent_preview_run(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    request: CodingAgentRunPreviewRequest,
) -> AppResult<CodingAgentRunPreview> {
    validate_automatic_execution_mode(request.execution_mode)?;
    let issue = crate::commands::issues::get_issue_detail(state, request.issue_id.clone()).await?;
    build_run_preview(&app.state::<AppState>(), issue, request).await
}

#[tauri::command]
pub async fn coding_agent_approve_and_start(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    request: ApproveAndStartCodingAgentRequest,
) -> AppResult<CodingAgentStartResponse> {
    validate_automatic_execution_mode(request.preview.execution_mode)?;
    if !request.user_confirmed {
        return Err(AppError::Config(
            "explicit user confirmation is required before approval".into(),
        ));
    }
    if request.preview.manual_repository.is_some() && !request.manual_repository_confirmed {
        return Err(AppError::Config(
            "manual repository selection requires explicit confirmation".into(),
        ));
    }
    if request.preview.auto_done_requested && !request.auto_done_confirmed {
        return Err(AppError::Config(
            "auto-DONE requires a separate explicit approval".into(),
        ));
    }
    if let Some(plan_id) = request.delegation_plan_id.as_deref() {
        let plan = state
            .coding_agent_run_store
            .delegation_plan(plan_id, &request.preview.issue_id)
            .map_err(|_| AppError::Config("delegation plan storage is unavailable".into()))?
            .ok_or_else(|| {
                AppError::Config("delegation plan was not found for this Issue".into())
            })?;
        validate_delegation_plan_for_approval(&plan, &request.preview)?;
    }
    state.ensure_coding_agent_accepting()?;
    let issue =
        crate::commands::issues::get_issue_detail(state, request.preview.issue_id.clone()).await?;
    let state = app.state::<AppState>();
    let preview = build_run_preview(&state, issue, request.preview).await?;
    if preview.plan_id != request.expected_plan_id || preview.digest != request.expected_digest {
        return Err(AppError::Config(
            "Issue, settings, or repository selection changed; review the new plan".into(),
        ));
    }
    if !preview.can_approve {
        return Ok(CodingAgentStartResponse {
            started: false,
            run_id: None,
            blockers: preview.blockers,
        });
    }
    let key = preview
        .preparation_key
        .as_deref()
        .ok_or_else(|| AppError::Config("approved execution preparation is missing".into()))?;
    let mut snapshot = state
        .coding_agent_execution
        .approved_candidate(key, &request.expected_digest)
        .await
        .map_err(|_| AppError::Config("approved execution preparation changed".into()))?;
    snapshot.auto_done = preview.auto_done_requested;
    snapshot.approval = Some(crate::coding_agent::RunApproval {
        approved_by: "lookback-user".into(),
        approved_at_ms: now_ms(),
        auto_done_approved: request.auto_done_confirmed,
        settings_version: Some(preview.settings_revision),
        applicable_modes: vec![ExecutionMode::OneShot],
    });
    let prompt = state
        .coding_agent_execution
        .prepared_prompt(&snapshot)
        .await
        .map_err(|_| AppError::Config("approved execution prompt changed".into()))?;
    let run_id = snapshot.run_id.clone();
    // An existing approval never becomes authority for a second dispatch.
    if state
        .coding_agent_coordinator
        .get_run(&run_id)
        .map_err(|_| AppError::Config("coding-agent run storage is unavailable".into()))?
        .is_none()
    {
        match state
            .coding_agent_coordinator
            .prepare_run_with_assessment_contract(snapshot, preview.assessment_contract)
            .await
            .map_err(|_| AppError::Config("coding-agent run preparation failed".into()))?
        {
            crate::coding_agent::PreparationOutcome::Ready(_) => {}
            crate::coding_agent::PreparationOutcome::Disabled { blockers } => {
                return Ok(CodingAgentStartResponse {
                    started: false,
                    run_id: None,
                    blockers,
                });
            }
        }
        state
            .coding_agent_coordinator
            .approve_run(&run_id)
            .map_err(|_| AppError::Config("coding-agent approval changed".into()))?;
    }
    if let Some(plan_id) = request.delegation_plan_id.as_deref() {
        link_delegation_for_start(&state.coding_agent_run_store, plan_id, &run_id)?;
    }
    let outcome = state
        .coding_agent_coordinator
        .start(&run_id, Some(prompt))
        .await
        .map_err(|_| {
            AppError::Config(
                "coding-agent start could not be confirmed; inspect the run before retrying".into(),
            )
        })?;
    match outcome {
        crate::coding_agent::StartOutcome::Started { .. } => Ok(CodingAgentStartResponse {
            started: true,
            run_id: Some(run_id.to_string()),
            blockers: vec![],
        }),
        crate::coding_agent::StartOutcome::Disabled { blockers } => Ok(CodingAgentStartResponse {
            started: false,
            run_id: Some(run_id.to_string()),
            blockers,
        }),
        crate::coding_agent::StartOutcome::Unknown { .. } => Ok(CodingAgentStartResponse {
            started: false,
            run_id: Some(run_id.to_string()),
            blockers: vec!["execution_start_unconfirmed".into()],
        }),
        _ => Ok(CodingAgentStartResponse {
            started: false,
            run_id: Some(run_id.to_string()),
            blockers: vec!["execution_already_claimed".into()],
        }),
    }
}

#[doc(hidden)]
pub fn link_delegation_for_start(
    store: &crate::coding_agent::store::RunStore,
    plan_id: &str,
    run_id: &RunId,
) -> AppResult<()> {
    // A preview may reuse an existing run; a failed link does not own its state.
    store
        .link_delegation_plan(plan_id, run_id)
        .map(|_| ())
        .map_err(|_| {
            AppError::Config(
                "delegation plan could not be linked; this request did not dispatch execution"
                    .into(),
            )
        })
}

#[tauri::command]
pub fn coding_agent_list_delegation_plans(
    state: State<'_, AppState>,
    issue_id: String,
) -> AppResult<Vec<crate::coding_agent::delegation::DelegationPlanRecord>> {
    state
        .coding_agent_run_store
        .delegation_plans(&issue_id)
        .map_err(|_| AppError::Config("coding-agent delegation plans are unavailable".into()))
}

#[doc(hidden)]
pub fn validate_delegation_plan_for_approval(
    plan: &crate::coding_agent::delegation::DelegationPlanRecord,
    preview: &CodingAgentRunPreviewRequest,
) -> AppResult<()> {
    if plan.run_id.is_some()
        || plan.request.issue_id != preview.issue_id
        || plan.request.profile_id != preview.profile_id
        || plan.request.purpose != preview.purpose
        || plan.request.prompt.as_deref().unwrap_or("") != preview.prompt.as_deref().unwrap_or("")
    {
        return Err(AppError::Config(
            "delegation plan changed or is already linked; inspect its run".into(),
        ));
    }
    Ok(())
}

#[tauri::command]
pub fn coding_agent_list_runs(state: State<'_, AppState>) -> AppResult<Vec<CodingAgentRunStatus>> {
    state
        .coding_agent_coordinator
        .recoverable_runs()
        .map_err(|_| AppError::Config("coding-agent run list is unavailable".into()))
        .map(|runs| {
            runs.iter()
                .map(|run| run_status(run, &state.coding_agent_coordinator))
                .collect()
        })
}

#[tauri::command]
pub fn coding_agent_get_run(
    state: State<'_, AppState>,
    run_id: String,
) -> AppResult<CodingAgentRunStatus> {
    let run_id = RunId::new(run_id)
        .map_err(|_| AppError::Config("coding-agent run identifier is invalid".into()))?;
    let run = state
        .coding_agent_coordinator
        .get_run(&run_id)
        .map_err(|_| AppError::Config("coding-agent run state is unavailable".into()))?
        .ok_or_else(|| AppError::Config("coding-agent run was not found".into()))?;
    Ok(run_status(&run, &state.coding_agent_coordinator))
}

#[tauri::command]
pub fn coding_agent_attach_run(
    state: State<'_, AppState>,
    window: WebviewWindow,
    run_id: String,
) -> AppResult<CodingAgentRunOwnerResponse> {
    let run = RunId::new(run_id)
        .map_err(|_| AppError::Config("coding-agent run identifier is invalid".into()))?;
    if state
        .coding_agent_coordinator
        .get_run(&run)
        .map_err(|_| AppError::Config("coding-agent run state is unavailable".into()))?
        .is_none()
    {
        return Err(AppError::Config("coding-agent run was not found".into()));
    }
    state.claim_coding_agent_run(run.as_str(), window.label())?;
    Ok(CodingAgentRunOwnerResponse {
        run_id: run.to_string(),
        window_label: window.label().to_owned(),
    })
}

#[tauri::command]
pub async fn coding_agent_cancel_run(
    state: State<'_, AppState>,
    window: WebviewWindow,
    run_id: String,
) -> AppResult<CodingAgentRunActionResponse> {
    let run_id = RunId::new(run_id)
        .map_err(|_| AppError::Config("coding-agent run identifier is invalid".into()))?;
    state.require_coding_agent_run_owner(run_id.as_str(), window.label())?;
    let run = state
        .coding_agent_coordinator
        .cancel(&run_id)
        .await
        .map_err(|_| AppError::Config("coding-agent cancellation is unconfirmed".into()))?;
    Ok(CodingAgentRunActionResponse {
        run: run_status(&run, &state.coding_agent_coordinator),
    })
}

#[tauri::command]
pub fn coding_agent_list_evidence(
    state: State<'_, AppState>,
    window: WebviewWindow,
    run_id: String,
) -> AppResult<Vec<CodingAgentEvidenceReference>> {
    let run_id = RunId::new(run_id)
        .map_err(|_| AppError::Config("coding-agent run identifier is invalid".into()))?;
    state.require_coding_agent_run_owner(run_id.as_str(), window.label())?;
    state
        .coding_agent_coordinator
        .evidence_references(&run_id)
        .map_err(|_| AppError::Config("coding-agent evidence list is unavailable".into()))
        .map(|references| references.iter().map(evidence_reference).collect())
}

#[tauri::command]
pub fn coding_agent_read_evidence(
    state: State<'_, AppState>,
    window: WebviewWindow,
    run_id: String,
    evidence_id: String,
) -> AppResult<CodingAgentEvidenceResponse> {
    let run_id = RunId::new(run_id)
        .map_err(|_| AppError::Config("coding-agent run identifier is invalid".into()))?;
    state.require_coding_agent_run_owner(run_id.as_str(), window.label())?;
    let reference = state
        .coding_agent_coordinator
        .evidence_references(&run_id)
        .map_err(|_| AppError::Config("coding-agent evidence list is unavailable".into()))?
        .into_iter()
        .find(|reference| reference.evidence_id == evidence_id)
        .ok_or_else(|| AppError::Config("coding-agent evidence was not found".into()))?;
    if reference.size_bytes > MAX_EVIDENCE_READ_BYTES {
        return Err(AppError::Config(
            "coding-agent evidence exceeds the read limit".into(),
        ));
    }
    let bytes = state
        .coding_agent_coordinator
        .read_evidence(&run_id, &reference.evidence_id)
        .map_err(|_| AppError::Config("coding-agent evidence is unavailable".into()))?;
    Ok(CodingAgentEvidenceResponse {
        evidence_id,
        media_type: reference.media_type,
        bytes,
    })
}

#[tauri::command]
pub async fn coding_agent_get_review(
    state: State<'_, AppState>,
    window: WebviewWindow,
    run_id: String,
) -> AppResult<CodingAgentReviewSnapshot> {
    let run_id = RunId::new(run_id)
        .map_err(|_| AppError::Config("coding-agent run identifier is invalid".into()))?;
    state.require_coding_agent_run_owner(run_id.as_str(), window.label())?;
    let record = require_awaiting_review(&state, &run_id)?;
    let service = state.issue_tracker_service().await?;
    let tracker = PrimaryIssueTrackerAdapter::new(service);
    let snapshot = tracker
        .read_issue_with_history(&record.snapshot.issue_id)
        .await
        .map_err(|_| AppError::IssueTrackerServer("primary Issue review is unavailable".into()))?;
    let marker = verified_review_marker(&record, &snapshot)?;
    Ok(CodingAgentReviewSnapshot {
        semantic_reassessment_available: record.snapshot.semantic_judge_enabled
            && record.snapshot.execution_mode == ExecutionMode::OneShot,
        run: run_status(&record, &state.coding_agent_coordinator),
        issue_state: issue_state_name(snapshot.state),
        issue_revision: snapshot.revision,
        marker,
    })
}

#[tauri::command]
pub async fn coding_agent_reassess(
    app: AppHandle,
    window: WebviewWindow,
    request: CodingAgentReassessmentRequest,
) -> AppResult<crate::coding_agent::coordinator::SemanticReassessmentRecord> {
    let state = app.state::<AppState>();
    let run_id = RunId::new(request.run_id.clone())
        .map_err(|_| AppError::Config("coding-agent run identifier is invalid".into()))?;
    state.require_coding_agent_run_owner(run_id.as_str(), window.label())?;
    let record = require_awaiting_review(&state, &run_id)?;
    if record.revision != request.expected_run_revision {
        return Err(AppError::Config(
            "coding-agent run changed; refresh the review".into(),
        ));
    }
    let tracker = PrimaryIssueTrackerAdapter::new(state.issue_tracker_service().await?);
    let current = tracker
        .read_issue_with_history(&record.snapshot.issue_id)
        .await
        .map_err(|_| AppError::IssueTrackerServer("primary Issue review is unavailable".into()))?;
    let marker = verified_review_marker(&record, &current)?;
    validate_reassessment_confirmation(&request, current.revision, &marker, current.state)?;
    if let Some(result) = state
        .coding_agent_coordinator
        .read_semantic_reassessment(
            &run_id,
            request.expected_run_revision,
            request.expected_issue_revision,
            &request.request_key,
        )
        .map_err(|_| AppError::Config("saved reassessment is unavailable".into()))?
    {
        return Ok(result);
    }
    let issue =
        crate::commands::issues::get_issue_detail(app.state(), record.snapshot.issue_id.clone())
            .await?;
    if issue.revision.parse::<u64>().ok() != Some(request.expected_issue_revision) {
        return Err(AppError::IssueTrackerServer(
            "Issue changed; refresh the review".into(),
        ));
    }
    state
        .coding_agent_execution
        .verify_reassessment_issue(&record.snapshot, &issue)
        .await
        .map_err(|_| AppError::Config("reassessment context changed or is unavailable".into()))?;
    let result = state
        .coding_agent_coordinator
        .reassess_semantics(
            &run_id,
            request.expected_run_revision,
            request.expected_issue_revision,
            &request.request_key,
        )
        .await
        .map_err(|_| {
            AppError::Config(
                "reassessment result is unavailable; inspect the run before retrying".into(),
            )
        })?;
    let current = tracker
        .read_issue_with_history(&record.snapshot.issue_id)
        .await
        .map_err(|_| AppError::IssueTrackerServer("primary Issue review is unavailable".into()))?;
    let marker = verified_review_marker(&record, &current)?;
    validate_reassessment_confirmation(&request, current.revision, &marker, current.state)?;
    if record.snapshot.auto_done {
        let authority = CompletionAuthority::new(
            state.coding_agent_run_store.clone(),
            Arc::new(tracker),
            CompletionPolicy::new(AUTO_COMPLETION_POLICY, 1)
                .map_err(|_| AppError::Config("auto-completion policy is unavailable".into()))?,
        )
        .map_err(|_| AppError::Config("auto-completion authority is unavailable".into()))?;
        // A valid saved judgment can remain available even if completion is blocked.
        let key = crate::coding_agent::completion::auto_completion_intent_key(
            run_id.as_str(),
            &result.attempt_id,
        );
        if let Ok(grant) = authority.authorize_auto_completion(&run_id, &key).await
            && let Err(error) = authority.complete_auto(grant).await
        {
            tracing::debug!(%error, "reassessment auto-completion remains unconfirmed");
        }
    }
    Ok(result)
}

#[tauri::command]
pub async fn coding_agent_apply_review_action(
    state: State<'_, AppState>,
    window: WebviewWindow,
    request: CodingAgentReviewActionRequest,
) -> AppResult<CodingAgentReviewActionResponse> {
    let run_id = RunId::new(request.run_id.clone())
        .map_err(|_| AppError::Config("coding-agent run identifier is invalid".into()))?;
    state.require_coding_agent_run_owner(run_id.as_str(), window.label())?;
    let record = require_awaiting_review(&state, &run_id)?;
    if record.revision != request.expected_run_revision {
        return Err(AppError::Config(
            "coding-agent run changed; refresh the review".into(),
        ));
    }
    let service = state.issue_tracker_service().await?;
    let tracker: Arc<dyn CompletionTracker> = Arc::new(PrimaryIssueTrackerAdapter::new(service));
    let current = tracker
        .read_issue_with_history(&record.snapshot.issue_id)
        .await
        .map_err(|_| AppError::IssueTrackerServer("primary Issue review is unavailable".into()))?;
    let marker = verified_review_marker(&record, &current)?;
    validate_review_confirmation(&request, current.revision, &marker, current.state)?;
    let target_state = match request.action {
        CodingAgentReviewAction::Complete => CompletionIssueState::Done,
        CodingAgentReviewAction::RequestChanges => CompletionIssueState::Open,
    };
    let exact_tracker = Arc::new(ExactReviewTracker {
        inner: tracker,
        run_id: run_id.clone(),
        issue_id: record.snapshot.issue_id.clone(),
        expected_revision: request.expected_issue_revision,
        expected_marker: request.expected_marker.clone(),
        target_state,
        transition_started: std::sync::atomic::AtomicBool::new(false),
    });
    let authority = CompletionAuthority::new(
        state.coding_agent_run_store.clone(),
        exact_tracker,
        CompletionPolicy::new(HUMAN_REVIEW_POLICY, 1)
            .map_err(|_| AppError::Config("human review policy is unavailable".into()))?,
    )
    .map_err(|_| AppError::Config("human review authority is unavailable".into()))?;
    let outcome = match request.action {
        CodingAgentReviewAction::Complete => {
            let report = request.report.as_ref().ok_or_else(|| {
                AppError::Config("verified completion evidence is required".into())
            })?;
            let approval = HumanReviewApproval::from_confirmed_review_dialog(
                run_id.clone(),
                request.rationale,
                verified_report(report)?,
            )
            .map_err(|_| AppError::Config("human review input is invalid".into()))?;
            authority.complete_human(approval).await
        }
        CodingAgentReviewAction::RequestChanges => {
            if request.report.is_some() {
                return Err(AppError::Config(
                    "request-changes actions cannot carry a completion report".into(),
                ));
            }
            let correction =
                HumanCorrectionRequest::from_review_dialog(run_id.clone(), request.rationale)
                    .map_err(|_| AppError::Config("human review input is invalid".into()))?;
            authority.request_correction(correction).await
        }
    }
    .map_err(|_| AppError::IssueTrackerServer("human review result is unconfirmed".into()))?;
    Ok(review_action_response(
        outcome,
        &state.coding_agent_coordinator,
    ))
}

/// Called by the app-owned periodic reconciler. The authority re-reads the
/// canonical Issue and history and accepts only its own exact prepared marker.
pub async fn reconcile_approved_auto_completion(state: &AppState) -> AppResult<usize> {
    let runs = state
        .coding_agent_coordinator
        .recoverable_runs()
        .map_err(|_| AppError::Config("coding-agent recovery state is unavailable".into()))?;
    let approved = runs
        .into_iter()
        .filter(|run| run.snapshot.auto_done && run.snapshot.semantic_judge_enabled)
        .filter(|run| run.state == RunState::AwaitingReview)
        .collect::<Vec<_>>();
    if approved.is_empty() {
        return Ok(0);
    }
    let service = state.issue_tracker_service().await?;
    let authority = CompletionAuthority::new(
        state.coding_agent_run_store.clone(),
        Arc::new(PrimaryIssueTrackerAdapter::new(service)),
        CompletionPolicy::new(AUTO_COMPLETION_POLICY, 1)
            .map_err(|_| AppError::Config("auto-completion policy is unavailable".into()))?,
    )
    .map_err(|_| AppError::Config("auto-completion authority is unavailable".into()))?;
    let run_ids = approved
        .into_iter()
        .map(|run| run.snapshot.run_id)
        .collect::<Vec<_>>();
    Ok(authority
        .reconcile_approved_auto_completions(&run_ids)
        .await)
}

/// Build the only approval assessment contract exposed by the native preview.
/// The canonical acceptance vector is supplied by the primary Issue read, not
/// reconstructed from the legacy newline-joined display string.
#[allow(clippy::too_many_arguments)]
pub fn build_approval_assessment_contract(
    purpose: RunPurpose,
    canonical_issue_acceptance: Vec<String>,
    issue_id: &str,
    selected_repository: Option<&RepositoryIdentitySettings>,
    criterion_policy_requests: Option<Vec<CodingAgentCriterionPolicyRequest>>,
    validation_draft_requests: Vec<CodingAgentValidationDraftRequest>,
    provider: Option<ProviderSelection>,
) -> Result<ApprovalAssessmentContract, &'static str> {
    build_profile_approval_assessment_contract(
        purpose,
        canonical_issue_acceptance,
        issue_id,
        selected_repository,
        criterion_policy_requests,
        validation_draft_requests,
        provider,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_profile_approval_assessment_contract(
    purpose: RunPurpose,
    canonical_issue_acceptance: Vec<String>,
    issue_id: &str,
    selected_repository: Option<&RepositoryIdentitySettings>,
    criterion_policy_requests: Option<Vec<CodingAgentCriterionPolicyRequest>>,
    validation_draft_requests: Vec<CodingAgentValidationDraftRequest>,
    provider: Option<ProviderSelection>,
    host_profile: Option<(&str, u64)>,
) -> Result<ApprovalAssessmentContract, &'static str> {
    if canonical_issue_acceptance.len() > 128 || validation_draft_requests.len() > 128 {
        return Err("assessment_items_exceed_limit");
    }
    let criterion_policies = match criterion_policy_requests {
        Some(requests) => {
            if requests.len() != canonical_issue_acceptance.len()
                || requests.len() > 128
                || requests
                    .iter()
                    .enumerate()
                    .any(|(index, request)| request.criterion_id != format!("AC-{}", index + 1))
            {
                return Err("criterion_policy_ids_must_match_canonical_acceptance");
            }
            requests
                .into_iter()
                .map(|request| ApprovalCriterionPolicy {
                    required: request.required,
                    method: request.method,
                })
                .collect::<Vec<_>>()
        }
        None => canonical_issue_acceptance
            .iter()
            .map(|_| ApprovalCriterionPolicy {
                required: true,
                method: if provider.is_some() {
                    CriterionEvaluationMethod::SemanticJudge
                } else {
                    CriterionEvaluationMethod::Unknown
                },
            })
            .collect(),
    };

    let validation_drafts = validation_draft_requests
        .into_iter()
        .enumerate()
        .map(|(index, draft)| {
            let command = if let Some(command) = draft.command {
                validate_validation_command_draft(&command)?;
                let (profile_id, version) = host_profile.ok_or("validation_profile_unavailable")?;
                let profile_digest =
                    crate::coding_agent::model::validation_profile_digest(profile_id, version)
                        .map_err(|_| "validation_profile_unavailable")?;
                Some(ApprovalAssessmentCommand {
                    argv: command.argv,
                    working_directory: command.working_directory,
                    timeout_ms: command.timeout_ms,
                    permissions: vec![
                        ValidationPermission::ReadWorkspace,
                        ValidationPermission::WriteBuildOutput,
                    ],
                    profile_digest,
                })
            } else {
                None
            };
            if index >= 128 {
                return Err("assessment_items_exceed_limit");
            }
            Ok(ApprovalValidationDraft {
                required: draft.required,
                command,
            })
        })
        .collect::<Result<Vec<_>, &'static str>>()?;

    let has_semantic_criteria = provider.is_some()
        || criterion_policies
            .iter()
            .any(|policy| policy.method == CriterionEvaluationMethod::SemanticJudge);
    let input_scope = approval_input_scope(
        issue_id,
        selected_repository,
        has_semantic_criteria,
        validation_drafts.len(),
    );
    let prompt_policy_version = provider
        .as_ref()
        .map(|_| APPROVED_JUDGE_PROMPT_POLICY_VERSION.to_owned());

    ApprovalAssessmentContract::from_issue_acceptance(
        purpose,
        canonical_issue_acceptance,
        criterion_policies,
        validation_drafts,
        provider,
        prompt_policy_version,
        input_scope,
    )
    .map_err(|_| "approval_assessment_contract_invalid")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodingAgentPreviewIssueState {
    Open,
    InProgress,
    Ineligible,
}

fn coding_agent_issue_state_blocker(issue_state: &str) -> Option<&'static str> {
    // The primary Issue DTO uses canonical uppercase names; accept only the two
    // exact lowercase spellings used by existing preview callers as legacy input.
    let state = match issue_state {
        "OPEN" | "open" => CodingAgentPreviewIssueState::Open,
        "IN_PROGRESS" | "in_progress" => CodingAgentPreviewIssueState::InProgress,
        _ => CodingAgentPreviewIssueState::Ineligible,
    };
    match state {
        CodingAgentPreviewIssueState::Open | CodingAgentPreviewIssueState::InProgress => None,
        CodingAgentPreviewIssueState::Ineligible => Some("issue_not_open_for_work"),
    }
}

fn validate_validation_command_draft(
    command: &CodingAgentValidationCommandRequest,
) -> Result<(), &'static str> {
    let argv_bytes = command
        .argv
        .iter()
        .try_fold(0usize, |total, argument| total.checked_add(argument.len()));
    if command.argv.is_empty()
        || command.argv[0].is_empty()
        || command.argv.len() > 128
        || command.argv.iter().any(|argument| {
            argument.len() > MAX_APPROVAL_TEXT_BYTES || argument.contains(char::from(0))
        })
        || argv_bytes.is_none_or(|bytes| bytes > MAX_APPROVAL_COMMAND_BYTES)
        || command.working_directory.len() > MAX_APPROVAL_TEXT_BYTES
        || (command.working_directory != "/workspace"
            && !command.working_directory.starts_with("/workspace/"))
        || command
            .working_directory
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b'\\')
        || command
            .working_directory
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
        || command.timeout_ms == 0
        || command.timeout_ms > crate::coding_agent::workspace_validation::MAX_VALIDATION_TIMEOUT_MS
    {
        return Err("validation_command_invalid");
    }
    Ok(())
}

fn approval_input_scope(
    issue_id: &str,
    selected_repository: Option<&RepositoryIdentitySettings>,
    has_semantic_criteria: bool,
    validation_count: usize,
) -> AssessmentInputScope {
    let mut allowed_fields = vec![
        ApprovedJudgeInputField::IssueTitle,
        ApprovedJudgeInputField::IssueBody,
    ];
    let mut allowed_sources = vec![AllowedEvidenceSource {
        kind: EvidenceSourceKind::IssueContext,
        identity: format!("issue:{issue_id}"),
        max_bytes: 32 * 1024,
    }];
    if let Some(repository) = selected_repository {
        allowed_fields.push(ApprovedJudgeInputField::RepositoryIdentity);
        allowed_sources.push(AllowedEvidenceSource {
            kind: EvidenceSourceKind::RepositoryContext,
            identity: format!(
                "repo:{}/{}/{}",
                repository.host, repository.organization, repository.repository
            ),
            max_bytes: 32 * 1024,
        });
    }
    if has_semantic_criteria {
        allowed_fields.push(ApprovedJudgeInputField::BoundedDiff);
        allowed_sources.push(AllowedEvidenceSource {
            kind: EvidenceSourceKind::Diff,
            identity: "workspace-source".into(),
            max_bytes: 32 * 1024,
        });
        allowed_fields.push(ApprovedJudgeInputField::Report);
        allowed_sources.push(AllowedEvidenceSource {
            kind: EvidenceSourceKind::Report,
            identity: "agent-report".into(),
            max_bytes: 1024 * 1024,
        });
    }
    if validation_count > 0 {
        allowed_fields.push(ApprovedJudgeInputField::ValidationResult);
        allowed_sources.extend((0..validation_count).map(|index| AllowedEvidenceSource {
            kind: EvidenceSourceKind::ValidationResult,
            identity: format!("V-{}", index + 1),
            max_bytes: 32 * 1024,
        }));
    }
    AssessmentInputScope {
        allowed_fields,
        allowed_sources,
        max_input_bytes: crate::coding_agent::assessment::MAX_JUDGE_INPUT_BYTES,
    }
}

fn llm_selection_digest(settings: &crate::commands::llm_settings::LlmSettings) -> String {
    let mut digest = Sha256::new();
    digest.update(b"jobworkerp-rs/coding-agent/selected-llm-config/v1\0");
    digest_optional_text(
        &mut digest,
        Some(match settings.mode {
            crate::commands::llm_settings::LlmMode::Local => "local",
            crate::commands::llm_settings::LlmMode::External => "external",
        }),
    );
    digest_optional_text(&mut digest, settings.provider_model.as_deref());
    digest_optional_text(&mut digest, settings.local_preset_id.as_deref());
    digest_optional_text(&mut digest, settings.local_model_file.as_deref());
    digest_optional_text(&mut digest, settings.local_hf_repo.as_deref());

    // Bind only a credential-free origin here; the actual supported worker
    // selection digest additionally binds its full checked configuration.
    let safe_origin = settings
        .base_url
        .as_deref()
        .and_then(safe_llm_base_url_origin);
    digest_optional_text(&mut digest, safe_origin.as_deref());
    digest_optional_text(
        &mut digest,
        settings.base_url.as_deref().map(|_| {
            if safe_origin.is_some() {
                "safe-origin"
            } else {
                "unsafe-redacted"
            }
        }),
    );
    format!("{:x}", digest.finalize())
}

fn safe_llm_base_url_origin(value: &str) -> Option<String> {
    let parsed = url::Url::parse(value).ok()?;
    if !matches!(parsed.scheme(), "https" | "http")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    Some(parsed.origin().ascii_serialization())
}

fn digest_optional_text(digest: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            digest.update([1]);
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        None => digest.update([0]),
    }
}

fn assessment_provider_blocker(
    semantic_judge_enabled: bool,
    contract: &ApprovalAssessmentContract,
) -> Option<&'static str> {
    (semantic_judge_enabled
        && contract.provider.is_none()
        && contract
            .criteria
            .iter()
            .any(|criterion| criterion.method == CriterionEvaluationMethod::SemanticJudge))
    .then_some("assessment_provider_unavailable")
}

fn validate_automatic_execution_mode(mode: ExecutionMode) -> AppResult<()> {
    if mode != ExecutionMode::OneShot {
        return Err(AppError::Config("automatic Issue execution requires one-shot mode; use the independent terminal for interactive work".into()));
    }
    Ok(())
}

async fn build_run_preview(
    state: &AppState,
    issue: crate::commands::issues::IssueListItem,
    request: CodingAgentRunPreviewRequest,
) -> AppResult<CodingAgentRunPreview> {
    validate_automatic_execution_mode(request.execution_mode)?;
    state.ensure_coding_agent_accepting()?;
    if request.issue_id != issue.id {
        return Err(AppError::IssueTrackerServer(
            "Issue identity changed while the plan was loaded".into(),
        ));
    }
    let issue_revision = issue
        .revision
        .parse::<u64>()
        .ok()
        .filter(|revision| *revision > 0)
        .ok_or_else(|| AppError::IssueTrackerServer("Issue revision is invalid".into()))?;
    let settings = state
        .coding_agent_settings
        .load()
        .map_err(map_settings_error)?;
    let profile = settings
        .profiles
        .iter()
        .find(|profile| profile.profile_id == request.profile_id);
    let mut blockers = Vec::new();
    if let Some(blocker) = coding_agent_issue_state_blocker(&issue.state) {
        blockers.push(blocker.into());
    }
    if request
        .prompt
        .as_ref()
        .is_some_and(|prompt| prompt.len() > MAX_PROMPT_BYTES || prompt.as_bytes().contains(&0))
    {
        blockers.push("prompt_invalid_or_too_large".into());
    }
    if profile.is_none() {
        blockers.push("profile_not_found".into());
    }
    let profile_version = profile.map(|profile| profile.version);
    if profile.is_some_and(|profile| profile.allowed_repositories.is_empty()) {
        blockers.push("profile_has_no_allowed_repositories".into());
    }
    if request.auto_done_requested {
        if !settings.auto_done_enabled {
            blockers.push("auto_done_preference_disabled".into());
        }
        if !settings.semantic_judge_enabled {
            blockers.push("semantic_assessment_disabled".into());
        }
    }

    let host_configs = settings
        .repository_hosts_core()
        .map_err(map_settings_error)?;
    let repository_context = issue
        .contexts
        .iter()
        .find(|context| format!("{:?}", context.kind) == "Repository")
        .and_then(|context| parse_repository_context(&context.value));
    let (resolved_repository, selection_reason, candidates) =
        if let Some(manual) = request.manual_repository.as_ref() {
            match manual.to_repository_identity() {
                Ok(identity) => {
                    let repository = crate::coding_agent::approve_manual_repository(
                        identity.clone(),
                        &issue.source_labels,
                        &host_configs,
                        "lookback-user",
                        now_ms(),
                    );
                    match repository {
                        Ok(repository) => (Some(repository), None, vec![identity]),
                        Err(_) => {
                            blockers.push("manual_repository_not_allowed".into());
                            (
                                None,
                                Some("manual_repository_not_allowed".into()),
                                vec![identity],
                            )
                        }
                    }
                }
                Err(_) => {
                    blockers.push("manual_repository_invalid".into());
                    (None, Some("manual_repository_invalid".into()), Vec::new())
                }
            }
        } else {
            match crate::coding_agent::resolve_repository(
                &issue.source_labels,
                &host_configs,
                repository_context.as_ref(),
            ) {
                RepositoryResolution::Ready(repository) => {
                    (Some(repository.clone()), None, vec![repository.identity])
                }
                RepositoryResolution::RequiresSelection {
                    reason, candidates, ..
                } => {
                    blockers.push(repository_selection_blocker(&reason).into());
                    (
                        None,
                        Some(repository_selection_reason(&reason).into()),
                        candidates,
                    )
                }
            }
        };
    let selected_repository =
        resolved_repository
            .as_ref()
            .map(|repository| RepositoryIdentitySettings {
                host: repository.identity.host.clone(),
                organization: repository.identity.organization.clone(),
                repository: repository.identity.repository.clone(),
            });
    if let (Some(profile), Some(repository)) = (profile, resolved_repository.as_ref()) {
        let allowed = profile
            .allowed_repositories
            .iter()
            .filter_map(|candidate| candidate.to_repository_identity().ok())
            .any(|candidate| candidate == repository.identity);
        if !allowed {
            blockers.push("repository_not_allowed_by_profile".into());
        }
    }

    let llm_settings = state.llm_settings_snapshot();
    let selected_provider =
        crate::coding_agent::semantic_judge::provider_selection(&llm_settings).ok();
    let llm_config_digest = llm_selection_digest(&llm_settings);
    let assessment_contract = build_profile_approval_assessment_contract(
        request.purpose,
        issue.acceptance_criteria_items.clone(),
        &issue.id,
        selected_repository.as_ref(),
        request.criterion_policies,
        request.validation_drafts,
        selected_provider,
        profile.map(|profile| (profile.profile_id.as_str(), profile.version)),
    )
    .map_err(|reason| {
        AppError::Config(format!("coding-agent assessment preview failed: {reason}"))
    })?;
    let assessment_contract_digest = assessment_contract.digest().map_err(|_| {
        AppError::Config("coding-agent assessment contract could not be encoded".into())
    })?;
    let assessment_provider_supported = assessment_contract.provider.is_some();
    if let Some(blocker) =
        assessment_provider_blocker(settings.semantic_judge_enabled, &assessment_contract)
    {
        blockers.push(blocker.into());
    }

    if !state.is_service_local(crate::commands::connection::Service::Jobworkerp) {
        blockers.push("app_managed_jobworkerp_required".into());
    }
    let prompt_sha256 = request
        .prompt
        .as_ref()
        .map(|prompt| format!("{:x}", Sha256::digest(prompt.as_bytes())));
    let repository_identity = resolved_repository
        .as_ref()
        .map(|repository| repository.identity.clone());
    let claims = PreviewClaims {
        issue_id: &issue.id,
        issue_revision,
        settings_revision: settings.revision,
        profile_id: &request.profile_id,
        profile_version,
        execution_mode: request.execution_mode,
        purpose: request.purpose,
        publication_plan: request.publication_plan.clone(),
        manual_repository: resolved_repository.as_ref().is_some_and(|repository| {
            repository.provenance.selection
                == crate::coding_agent::RepositorySelection::ManualApproved
        }),
        repository: repository_identity,
        prompt_sha256,
        auto_done_requested: request.auto_done_requested,
        canonical_acceptance_criteria: &issue.acceptance_criteria_items,
        assessment_contract_digest: &assessment_contract_digest,
        llm_selection_digest: &llm_config_digest,
    };
    let preparation_key = preview_claim_digest(&claims)
        .map_err(|_| AppError::Config("coding-agent plan could not be encoded".into()))?;
    let prepared = if blockers.is_empty() {
        match state
            .coding_agent_execution
            .preview_candidate(
                &preparation_key,
                crate::native_terminal::wire::NativeTerminalPrepareRequest {
                    issue_id: issue.id.clone(),
                    profile_id: request.profile_id.clone(),
                    base_branch: String::new(),
                    prompt: request.prompt.clone(),
                },
                settings.semantic_judge_enabled,
                request.publication_plan.clone(),
                &assessment_contract,
                resolved_repository
                    .as_ref()
                    .filter(|repository| {
                        repository.provenance.selection
                            == crate::coding_agent::RepositorySelection::ManualApproved
                    })
                    .cloned(),
            )
            .await
        {
            Ok(snapshot)
                if snapshot.issue_revision == issue_revision
                    && resolved_repository.as_ref().is_some_and(|repository| {
                        same_prepared_repository(repository, &snapshot.repository)
                    }) =>
            {
                if snapshot.publication_plan != PublicationPlan::NoPublication {
                    match crate::coding_agent::PublicationAdapter::preflight(
                        state.coding_agent_execution.as_ref(),
                        &snapshot,
                    )
                    .await
                    {
                        Ok(support) => blockers.extend(
                            crate::coding_agent::publication::validate_publication_support(
                                &snapshot, &support,
                            ),
                        ),
                        Err(_) => blockers.push("publication_adapter_unavailable".into()),
                    }
                }
                Some(snapshot)
            }
            Ok(_) => {
                blockers.push("canonical_execution_snapshot_changed".into());
                None
            }
            Err(_) => {
                blockers.push("automatic_workspace_or_runtime_unavailable".into());
                None
            }
        }
    } else {
        None
    };
    let digest = match &prepared {
        Some(snapshot) => crate::coding_agent::native_execution::prepared_snapshot_digest(
            &preparation_key,
            snapshot,
        )
        .map_err(|_| AppError::Config("prepared coding-agent plan could not be encoded".into()))?,
        None => preparation_key.clone(),
    };
    blockers.sort();
    blockers.dedup();
    let plan_id = digest[..32].to_owned();
    let repository_candidates = candidates
        .into_iter()
        .map(|identity| RepositoryIdentitySettings {
            host: identity.host,
            organization: identity.organization,
            repository: identity.repository,
        })
        .collect();
    let can_approve = blockers.is_empty();
    Ok(CodingAgentRunPreview {
        plan_id,
        digest,
        issue_id: issue.id,
        issue_revision,
        issue_title: issue.title,
        issue_state: issue.state,
        source_labels: issue.source_labels,
        settings_revision: settings.revision,
        profile_id: profile.map(|profile| profile.profile_id.clone()),
        profile_version,
        execution_mode: request.execution_mode,
        purpose: request.purpose,
        publication_plan: request.publication_plan,
        selected_repository,
        repository_selection_reason: selection_reason,
        repository_candidates,
        auto_done_requested: request.auto_done_requested,
        assessment_contract,
        assessment_contract_digest,
        assessment_provider_supported,
        blockers,
        can_approve,
        base_commit: prepared
            .as_ref()
            .map(|snapshot| snapshot.base_commit.clone()),
        workspace_branch: prepared
            .as_ref()
            .map(|snapshot| snapshot.workspace_branch.clone()),
        image_digest: prepared
            .as_ref()
            .map(|snapshot| snapshot.profile.image_digest.clone()),
        preparation_key: prepared.map(|_| preparation_key),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreviewClaims<'a> {
    issue_id: &'a str,
    issue_revision: u64,
    settings_revision: u64,
    profile_id: &'a str,
    profile_version: Option<u64>,
    execution_mode: ExecutionMode,
    purpose: RunPurpose,
    publication_plan: PublicationPlan,
    repository: Option<RepositoryIdentity>,
    manual_repository: bool,
    prompt_sha256: Option<String>,
    auto_done_requested: bool,
    canonical_acceptance_criteria: &'a [String],
    assessment_contract_digest: &'a str,
    llm_selection_digest: &'a str,
}

fn same_prepared_repository(
    current: &crate::coding_agent::ResolvedRepository,
    prepared: &crate::coding_agent::ResolvedRepository,
) -> bool {
    let mut current = current.clone();
    if current.provenance.selection == crate::coding_agent::RepositorySelection::ManualApproved
        && prepared.provenance.selection == crate::coding_agent::RepositorySelection::ManualApproved
    {
        current.provenance.approved_at_ms = prepared.provenance.approved_at_ms;
    }
    current == *prepared
}

fn preview_claim_digest(claims: &PreviewClaims<'_>) -> Result<String, serde_json::Error> {
    let claims_bytes = serde_json::to_vec(claims)?;
    Ok(format!("{:x}", Sha256::digest(claims_bytes)))
}

fn parse_repository_context(value: &str) -> Option<RepositoryIdentity> {
    let value = value.strip_prefix("repo:").unwrap_or(value);
    let mut parts = value.split('/');
    let identity = RepositoryIdentity::new(parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    identity.ok()
}

fn repository_selection_blocker(reason: &RepositorySelectionReason) -> &'static str {
    match reason {
        RepositorySelectionReason::MissingLabel => "repository_label_missing",
        RepositorySelectionReason::InvalidLabel => "repository_label_invalid",
        RepositorySelectionReason::UnconfiguredHost => "repository_host_unconfigured",
        RepositorySelectionReason::Ambiguous => "repository_selection_ambiguous",
        RepositorySelectionReason::ContextConflict => "repository_context_conflict",
        RepositorySelectionReason::InvalidHostConfiguration => {
            "repository_host_configuration_invalid"
        }
    }
}

fn repository_selection_reason(reason: &RepositorySelectionReason) -> &'static str {
    match reason {
        RepositorySelectionReason::MissingLabel => "missing_label",
        RepositorySelectionReason::InvalidLabel => "invalid_label",
        RepositorySelectionReason::UnconfiguredHost => "unconfigured_host",
        RepositorySelectionReason::Ambiguous => "ambiguous",
        RepositorySelectionReason::ContextConflict => "context_conflict",
        RepositorySelectionReason::InvalidHostConfiguration => "invalid_host_configuration",
    }
}

fn run_status(
    run: &AgentRunRecord,
    coordinator: &crate::coding_agent::CodingAgentCoordinator,
) -> CodingAgentRunStatus {
    CodingAgentRunStatus {
        run_id: run.snapshot.run_id.to_string(),
        issue_id: run.snapshot.issue_id.clone(),
        issue_revision: run.snapshot.issue_revision,
        revision: run.revision,
        execution_mode: run.snapshot.execution_mode,
        purpose: run.snapshot.purpose,
        state: run.state,
        stage: run.stage,
        profile_id: run.snapshot.profile.profile_id.clone(),
        profile_version: run.snapshot.profile.version,
        current_attempt_id: run.current_attempt_id.clone(),
        auto_done: run.snapshot.auto_done,
        workspace_path: coordinator
            .workspace_receipt(run)
            .ok()
            .flatten()
            .map(|receipt| receipt.workspace_token),
        workspace_branch: run.snapshot.workspace_branch.clone(),
        commit_id: run
            .execution_evidence
            .as_ref()
            .filter(|evidence| evidence.commit_consistent)
            .and_then(|evidence| evidence.commit_id.clone()),
        stdout_bytes: run
            .execution_evidence
            .as_ref()
            .map(|evidence| evidence.stdout_bytes),
        stderr_bytes: run
            .execution_evidence
            .as_ref()
            .map(|evidence| evidence.stderr_bytes),
        evidence: run.evidence.iter().map(evidence_reference).collect(),
    }
}

fn evidence_reference(
    reference: &crate::coding_agent::EvidenceReference,
) -> CodingAgentEvidenceReference {
    CodingAgentEvidenceReference {
        evidence_id: reference.evidence_id.clone(),
        attempt_id: reference.attempt_id.clone(),
        name: reference.name.clone(),
        media_type: reference.media_type.clone(),
        sha256: reference.sha256.clone(),
        size_bytes: reference.size_bytes,
        save_confirmed: reference.save_confirmed,
        created_at_ms: reference.created_at_ms,
    }
}

fn require_awaiting_review(state: &AppState, run_id: &RunId) -> AppResult<AgentRunRecord> {
    let record = state
        .coding_agent_coordinator
        .get_run(run_id)
        .map_err(|_| AppError::Config("coding-agent run state is unavailable".into()))?
        .ok_or_else(|| AppError::Config("coding-agent run was not found".into()))?;
    if record.state != RunState::AwaitingReview {
        return Err(AppError::Config(
            "coding-agent run is not awaiting human review".into(),
        ));
    }
    Ok(record)
}

fn verified_review_marker(
    record: &AgentRunRecord,
    snapshot: &TrackerIssueSnapshot,
) -> AppResult<CodingAgentReviewMarker> {
    if snapshot.issue_id != record.snapshot.issue_id
        || snapshot.state != CompletionIssueState::AwaitingReview
    {
        return Err(AppError::IssueTrackerServer(
            "Issue is no longer awaiting review".into(),
        ));
    }
    let expected_commit = record
        .execution_evidence
        .as_ref()
        .and_then(|assessment| assessment.commit_id.as_deref())
        .ok_or_else(|| {
            AppError::Config("coding-agent frozen commit evidence is unavailable".into())
        })?;
    let marker = snapshot
        .history
        .iter()
        .filter(|entry| {
            entry.issue_id == record.snapshot.issue_id
                && entry.new_state == CompletionIssueState::AwaitingReview
        })
        .max_by_key(|entry| entry.resulting_revision)
        .and_then(|entry| entry.awaiting_review.as_ref())
        .ok_or_else(|| {
            AppError::IssueTrackerServer("canonical review marker was not found".into())
        })?;
    if marker.full_commit_id != expected_commit || marker.evidence_references.is_empty() {
        return Err(AppError::IssueTrackerServer(
            "canonical review marker does not match this run".into(),
        ));
    }
    Ok(CodingAgentReviewMarker {
        run_id: marker.run_id.clone(),
        full_commit_id: marker.full_commit_id.clone(),
        evidence_references: marker.evidence_references.clone(),
    })
}

fn validate_review_confirmation(
    request: &CodingAgentReviewActionRequest,
    current_revision: u64,
    current_marker: &CodingAgentReviewMarker,
    current_state: CompletionIssueState,
) -> AppResult<()> {
    validate_review_context(
        request.expected_issue_revision,
        &request.expected_marker,
        current_revision,
        current_marker,
        current_state,
    )
}

fn validate_reassessment_confirmation(
    request: &CodingAgentReassessmentRequest,
    current_revision: u64,
    current_marker: &CodingAgentReviewMarker,
    current_state: CompletionIssueState,
) -> AppResult<()> {
    if !crate::coding_agent::delegation::valid_identifier(&request.request_key, 512)
        || request.expected_marker.run_id != request.run_id
        || current_marker.run_id != request.run_id
    {
        return Err(AppError::Config(
            "human reassessment input is invalid".into(),
        ));
    }
    validate_review_context(
        request.expected_issue_revision,
        &request.expected_marker,
        current_revision,
        current_marker,
        current_state,
    )
}

fn validate_review_context(
    expected_revision: u64,
    expected_marker: &CodingAgentReviewMarker,
    current_revision: u64,
    current_marker: &CodingAgentReviewMarker,
    current_state: CompletionIssueState,
) -> AppResult<()> {
    if current_revision != expected_revision
        || current_marker != expected_marker
        || current_state != CompletionIssueState::AwaitingReview
    {
        return Err(AppError::IssueTrackerServer(
            "Issue or review marker changed; refresh before acting".into(),
        ));
    }
    Ok(())
}

fn verified_report(
    report: &CodingAgentReviewReportInput,
) -> AppResult<crate::coding_agent::DurableCompletionReport> {
    use crate::grpc::proto::issue_tracker::v1 as issue;
    if report.evidence.is_empty() || report.evidence.len() > 1000 {
        return Err(AppError::Config(
            "verified completion requires bounded evidence".into(),
        ));
    }
    let evidence = report
        .evidence
        .iter()
        .map(|item| {
            if item.summary.trim().is_empty()
                || item.summary.len() > 4096
                || item.summary.chars().any(char::is_control)
                || item.uri.as_ref().is_some_and(|uri| {
                    uri.trim().is_empty() || uri.len() > 4096 || uri.chars().any(char::is_control)
                })
            {
                return Err(AppError::Config("completion evidence is invalid".into()));
            }
            let kind = match item.kind {
                CodingAgentReviewEvidenceKind::TestResult => {
                    issue::CompletionEvidenceKind::TestResult
                }
                CodingAgentReviewEvidenceKind::Artifact => issue::CompletionEvidenceKind::Artifact,
                CodingAgentReviewEvidenceKind::OtherEvidence => {
                    issue::CompletionEvidenceKind::OtherEvidence
                }
            };
            Ok(issue::CompletionEvidence {
                kind: kind as i32,
                summary: item.summary.clone(),
                uri: item.uri.clone(),
            })
        })
        .collect::<AppResult<Vec<_>>>()?;
    Ok(issue::CompletionReport {
        result: Some(issue::completion_report::Result::Verified(
            issue::VerifiedCompletion { evidence },
        )),
    })
}

fn review_action_response(
    outcome: CompletionOutcome,
    coordinator: &crate::coding_agent::CodingAgentCoordinator,
) -> CodingAgentReviewActionResponse {
    match outcome {
        CompletionOutcome::Completed {
            run,
            issue_revision,
        } => CodingAgentReviewActionResponse {
            outcome: "completed".into(),
            run: Some(run_status(&run, coordinator)),
            issue_revision,
        },
        CompletionOutcome::CorrectionRequested { issue_revision, .. } => {
            CodingAgentReviewActionResponse {
                outcome: "correction_requested".into(),
                run: None,
                issue_revision,
            }
        }
    }
}

fn issue_state_name(state: CompletionIssueState) -> String {
    match state {
        CompletionIssueState::Open => "open",
        CompletionIssueState::InProgress => "in_progress",
        CompletionIssueState::Blocked => "blocked",
        CompletionIssueState::Deferred => "deferred",
        CompletionIssueState::Done => "done",
        CompletionIssueState::Canceled => "canceled",
        CompletionIssueState::AwaitingReview => "awaiting_review",
        CompletionIssueState::Unknown => "unknown",
    }
    .into()
}

fn map_settings_error(error: SettingsError) -> AppError {
    AppError::Config(error.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::run_blocking_work;

    #[tokio::test]
    async fn blocking_work_runs_off_the_calling_thread() {
        let caller = std::thread::current().id();
        let worker = run_blocking_work(|| std::thread::current().id())
            .await
            .unwrap();
        assert_ne!(worker, caller);
    }

    #[tokio::test]
    async fn blocking_work_reports_a_panicked_task_as_an_error() {
        let result = run_blocking_work(|| -> u8 { panic!("registry work panicked") }).await;
        assert!(result.is_err());
    }

    #[test]
    fn native_setup_response_keeps_the_exact_renderer_wire_shape() {
        use crate::native_terminal::registry::{
            NativeSetupApprovalSource, NativeSetupPackageProvenance, NativeSetupSnapshot,
            NativeSetupUnavailableReason,
        };
        let snapshot = NativeSetupSnapshot {
            settings_revision: 7,
            state: NativeSetupState::Approved,
            package_fingerprint: Some("a".repeat(64)),
            package_version: Some("0.1.0".into()),
            repository_approvals: Vec::new(),
            unavailable_reason: None,
            package_provenance: Some(NativeSetupPackageProvenance::Development),
            approval_source: NativeSetupApprovalSource::Gui,
        };
        let wire = serde_json::to_value(native_setup_response(snapshot.clone())).unwrap();
        assert_eq!(wire["settingsRevision"], 7);
        assert_eq!(wire["state"], "approved");
        assert_eq!(wire["packageFingerprint"].as_str().unwrap().len(), 64);
        assert_eq!(wire["packageVersion"], "0.1.0");
        assert!(wire.get("repositoryApprovals").is_some());
        assert_eq!(wire["unavailableReason"], serde_json::Value::Null);
        assert_eq!(wire["packageProvenance"], "development");
        assert_eq!(wire["approvalSource"], "gui");
        assert!(wire.get("settings_revision").is_none());
        assert!(wire.get("unavailable_reason").is_none());

        let reapproval = serde_json::to_value(native_setup_response(NativeSetupSnapshot {
            state: NativeSetupState::ReapprovalRequired,
            ..snapshot.clone()
        }))
        .unwrap();
        assert_eq!(reapproval["state"], "reapproval_required");

        let unavailable = serde_json::to_value(native_setup_response(NativeSetupSnapshot {
            state: NativeSetupState::Unavailable,
            package_fingerprint: None,
            package_version: None,
            unavailable_reason: Some(NativeSetupUnavailableReason::PackageMissing),
            package_provenance: None,
            approval_source: NativeSetupApprovalSource::LegacyPolicy,
            ..snapshot
        }))
        .unwrap();
        assert_eq!(unavailable["state"], "unavailable");
        assert_eq!(unavailable["unavailableReason"], "package_missing");
        assert_eq!(unavailable["packageProvenance"], serde_json::Value::Null);
        assert_eq!(unavailable["packageVersion"], serde_json::Value::Null);
        assert_eq!(unavailable["approvalSource"], "legacy_policy");
    }

    #[test]
    fn settings_response_includes_read_only_agent_presets_without_changing_settings() {
        let settings = CodingAgentSettings::default();
        let response = settings_response(settings.clone(), Path::new("/"));

        assert_eq!(response.settings, settings);
        assert!(!response.available_agent_presets.is_empty());
        let wire = serde_json::to_value(response).expect("settings response is serializable");
        assert!(wire.get("availableAgentPresets").is_some());
        assert!(wire.get("available_agent_presets").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn settings_response_uses_the_exact_store_root_for_managed_home_availability() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let managed_root = temp.path().join("profile-homes");
        let managed_home =
            managed_root.join(crate::coding_agent::assessment::sha256_hex(b"profile"));
        let shared_root = temp.path().join("profile-shared-homes");
        let shared_home = shared_root.join(crate::coding_agent::assessment::sha256_hex(b"profile"));
        fs::create_dir_all(&managed_home).unwrap();
        fs::create_dir_all(&shared_home).unwrap();
        for path in [&managed_root, &managed_home, &shared_root, &shared_home] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let image = "registry.example/agent@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut settings = CodingAgentSettings::default();
        settings
            .profiles
            .push(crate::coding_agent::settings::AgentProfileSettings {
                profile_id: "profile".into(),
                version: 1,
                display_name: "Profile".into(),
                image_ref: image.into(),
                allowed_image_refs: vec![image.into()],
                command: vec!["agent".into()],
                container_home: "/home/agent".into(),
                host_configuration: Some(Default::default()),
                runtime_environment: Default::default(),
                resources: Default::default(),
                network_access: Default::default(),
                mount_allowed_roots: vec![managed_home.clone()],
                persistent_mounts: vec![crate::coding_agent::settings::PersistentMountSettings {
                    host_path: managed_home,
                    guest_path: "/home/agent".into(),
                    read_only: false,
                    executable: false,
                }],
                allowed_repositories: Vec::new(),
                credential_reference: None,
            });

        let response = settings_response(settings, &managed_root);
        assert!(response.profile_mount_availability[0].available);
    }

    #[test]
    fn automatic_issue_entry_accepts_one_shot_and_rejects_interactive() {
        assert!(validate_automatic_execution_mode(ExecutionMode::OneShot).is_ok());
        assert!(validate_automatic_execution_mode(ExecutionMode::Interactive).is_err());
    }

    #[test]
    fn human_review_preview_does_not_require_a_semantic_provider() {
        for purpose in [RunPurpose::Implementation, RunPurpose::Investigation] {
            let contract = build_approval_assessment_contract(
                purpose,
                vec!["The Issue is resolved".into()],
                "issue-42",
                None,
                Some(vec![CodingAgentCriterionPolicyRequest {
                    criterion_id: "AC-1".into(),
                    required: true,
                    method: CriterionEvaluationMethod::SemanticJudge,
                }]),
                Vec::new(),
                None,
            )
            .unwrap();
            assert!(assessment_provider_blocker(false, &contract).is_none());
            assert!(assessment_provider_blocker(true, &contract).is_some());
        }
    }

    #[test]
    fn an_issue_without_acceptance_items_still_approves_whole_issue_semantic_evidence() {
        let provider = crate::coding_agent::semantic_judge::provider_selection(
            &crate::commands::llm_settings::LlmSettings::default(),
        )
        .unwrap();
        let contract = build_approval_assessment_contract(
            RunPurpose::Investigation,
            vec![],
            "issue-1",
            None,
            None,
            vec![],
            Some(provider),
        )
        .unwrap();
        assert!(
            contract
                .input_scope
                .allowed_fields
                .contains(&ApprovedJudgeInputField::BoundedDiff)
        );
        assert!(
            contract
                .input_scope
                .allowed_fields
                .contains(&ApprovedJudgeInputField::Report)
        );
    }

    #[test]
    fn semantic_approval_permits_the_mounted_source_observation_as_judge_evidence() {
        let scope = approval_input_scope("issue-42", None, true, 0);
        assert!(
            scope
                .allowed_fields
                .contains(&ApprovedJudgeInputField::BoundedDiff)
        );
        assert!(scope.allowed_sources.iter().any(|source| {
            source.kind == EvidenceSourceKind::Diff && source.identity == "workspace-source"
        }));
        let human_only = approval_input_scope("issue-42", None, false, 0);
        assert!(
            !human_only
                .allowed_fields
                .contains(&ApprovedJudgeInputField::BoundedDiff)
        );
        assert!(
            !human_only
                .allowed_sources
                .iter()
                .any(|source| source.kind == EvidenceSourceKind::Diff)
        );
    }
    use super::*;

    struct StaticCompletionTracker(TrackerIssueSnapshot);

    #[async_trait::async_trait]
    impl CompletionTracker for StaticCompletionTracker {
        async fn read_issue_with_history(
            &self,
            _issue_id: &str,
        ) -> crate::coding_agent::TrackerResult<TrackerIssueSnapshot> {
            Ok(self.0.clone())
        }

        fn primary_identity(
            &self,
        ) -> crate::coding_agent::TrackerResult<crate::coding_agent::model::PrimaryTrackerIdentity>
        {
            Ok(crate::coding_agent::model::PrimaryTrackerIdentity {
                schema_fingerprint: "schema".into(),
                data_root_id: "root".into(),
                contract_fingerprint: "contract".into(),
            })
        }

        async fn transition_issue(
            &self,
            _request: TrackerMutationRequest,
        ) -> crate::coding_agent::TrackerResult<()> {
            Err(TrackerError::Rejected)
        }

        async fn transition_issue_with_receipt(
            &self,
            _request: TrackerMutationRequest,
        ) -> crate::coding_agent::TrackerResult<crate::coding_agent::model::PrimaryOperationReceipt>
        {
            Err(TrackerError::Rejected)
        }

        async fn lookup_transition_operation(
            &self,
            _authorization: &RunCompletionAuthorization,
        ) -> crate::coding_agent::completion::PrimaryOperationLookup {
            crate::coding_agent::completion::PrimaryOperationLookup::Unknown
        }
    }

    #[test]
    fn semantic_reassessment_requires_current_human_review_context_and_strict_input() {
        let marker = CodingAgentReviewMarker {
            run_id: "run-1".into(),
            full_commit_id: "a".repeat(40),
            evidence_references: vec!["evidence-1".into()],
        };
        let mut request: CodingAgentReassessmentRequest =
            serde_json::from_value(serde_json::json!({
                "runId": "run-1", "expectedRunRevision": 3, "expectedIssueRevision": 9,
                "expectedMarker": marker.clone(), "requestKey": "human-request-1",
            }))
            .unwrap();
        assert!(
            validate_reassessment_confirmation(
                &request,
                9,
                &marker,
                CompletionIssueState::AwaitingReview
            )
            .is_ok()
        );
        assert!(
            validate_reassessment_confirmation(
                &request,
                10,
                &marker,
                CompletionIssueState::AwaitingReview
            )
            .is_err()
        );
        assert!(
            validate_reassessment_confirmation(&request, 9, &marker, CompletionIssueState::Done)
                .is_err()
        );
        request.expected_marker.run_id = "other-run".into();
        assert!(
            validate_reassessment_confirmation(
                &request,
                9,
                &marker,
                CompletionIssueState::AwaitingReview
            )
            .is_err()
        );
        request.expected_marker = marker.clone();
        request.request_key.clear();
        assert!(
            validate_reassessment_confirmation(
                &request,
                9,
                &marker,
                CompletionIssueState::AwaitingReview
            )
            .is_err()
        );
        request.request_key = "valid-key".into();
        request.request_key = "x".repeat(513);
        assert!(
            validate_reassessment_confirmation(
                &request,
                9,
                &marker,
                CompletionIssueState::AwaitingReview
            )
            .is_err()
        );
        assert!(serde_json::from_value::<CodingAgentReassessmentRequest>(serde_json::json!({
            "runId":"run-1", "expectedRunRevision":3,"expectedIssueRevision":9,"expectedMarker":marker,
            "requestKey":"valid-key", "approved":true,
        })).is_err());
    }

    #[test]
    fn human_review_rejects_changed_issue_revision_or_marker() {
        let marker = CodingAgentReviewMarker {
            run_id: "run-1".into(),
            full_commit_id: "a".repeat(40),
            evidence_references: vec!["evidence-1".into()],
        };
        let request = CodingAgentReviewActionRequest {
            run_id: "run-1".into(),
            expected_run_revision: 3,
            expected_issue_revision: 9,
            expected_marker: marker.clone(),
            action: CodingAgentReviewAction::Complete,
            rationale: "reviewed".into(),
            report: None,
        };
        assert!(
            validate_review_confirmation(
                &request,
                9,
                &marker,
                CompletionIssueState::AwaitingReview
            )
            .is_ok()
        );
        assert!(
            validate_review_confirmation(
                &request,
                10,
                &marker,
                CompletionIssueState::AwaitingReview
            )
            .is_err()
        );
        let changed_marker = CodingAgentReviewMarker {
            evidence_references: vec!["different-evidence".into()],
            ..marker.clone()
        };
        assert!(
            validate_review_confirmation(
                &request,
                9,
                &changed_marker,
                CompletionIssueState::AwaitingReview
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn authority_read_rejects_issue_revision_changed_after_dialog_confirmation() {
        let marker = CodingAgentReviewMarker {
            run_id: "run-1".into(),
            full_commit_id: "a".repeat(40),
            evidence_references: vec!["evidence-1".into()],
        };
        let snapshot = TrackerIssueSnapshot {
            issue_id: "issue-1".into(),
            state: CompletionIssueState::AwaitingReview,
            revision: 10,
            history: vec![crate::coding_agent::IssueHistoryEntry {
                issue_id: "issue-1".into(),
                resulting_revision: 9,
                old_state: Some(CompletionIssueState::InProgress),
                new_state: CompletionIssueState::AwaitingReview,
                awaiting_review: Some(crate::coding_agent::AwaitingReviewMarker {
                    run_id: marker.run_id.clone(),
                    full_commit_id: marker.full_commit_id.clone(),
                    evidence_references: marker.evidence_references.clone(),
                }),
                completion: None,
                actor_id: "agent".into(),
                actor_kind: "system".into(),
                correlation_id: "review-marker".into(),
                reason: "ready for review".into(),
            }],
        };
        let tracker = ExactReviewTracker {
            inner: Arc::new(StaticCompletionTracker(snapshot)),
            run_id: RunId::new("run-1").unwrap(),
            issue_id: "issue-1".into(),
            expected_revision: 9,
            expected_marker: marker,
            target_state: CompletionIssueState::Done,
            transition_started: std::sync::atomic::AtomicBool::new(false),
        };

        assert_eq!(
            tracker
                .read_issue_with_history("issue-1")
                .await
                .unwrap_err(),
            TrackerError::Unconfirmed,
            "a new Issue revision after the dialog snapshot must revoke the authorization"
        );
        assert_eq!(
            tracker
                .read_issue_with_operation_history("issue-1")
                .await
                .unwrap_err(),
            TrackerError::Unconfirmed,
            "operation-history reads must retain the exact review revision gate"
        );
    }

    #[test]
    fn preview_digest_binds_canonical_acceptance_contract_and_selected_llm_config() {
        let acceptance = vec!["first condition\nwith exact newline".to_owned()];
        let mut claims = PreviewClaims {
            issue_id: "issue-1",
            issue_revision: 4,
            settings_revision: 5,
            profile_id: "profile-1",
            profile_version: Some(2),
            execution_mode: ExecutionMode::OneShot,
            purpose: RunPurpose::Implementation,
            publication_plan: PublicationPlan::NoPublication,
            manual_repository: false,
            repository: None,
            prompt_sha256: None,
            auto_done_requested: false,
            canonical_acceptance_criteria: &acceptance,
            assessment_contract_digest: "contract-digest",
            llm_selection_digest: "llm-config-a",
        };
        let original = preview_claim_digest(&claims).unwrap();

        claims.assessment_contract_digest = "changed-contract";
        assert_ne!(original, preview_claim_digest(&claims).unwrap());
        claims.assessment_contract_digest = "contract-digest";
        claims.llm_selection_digest = "llm-config-b";
        assert_ne!(original, preview_claim_digest(&claims).unwrap());
        claims.llm_selection_digest = "llm-config-a";
        claims.profile_version = Some(3);
        assert_ne!(original, preview_claim_digest(&claims).unwrap());
        claims.profile_version = Some(2);
        claims.settings_revision += 1;
        assert_ne!(original, preview_claim_digest(&claims).unwrap());

        let changed_acceptance = vec!["second condition".to_owned()];
        claims.settings_revision -= 1;
        claims.llm_selection_digest = "llm-config-a";
        claims.canonical_acceptance_criteria = &changed_acceptance;
        assert_ne!(original, preview_claim_digest(&claims).unwrap());
    }

    #[test]
    fn unsupported_llm_selection_changes_are_bound_without_retaining_unsafe_urls() {
        let mut settings = crate::commands::llm_settings::LlmSettings {
            mode: crate::commands::llm_settings::LlmMode::External,
            provider_model: Some("unversioned-model-alias".into()),
            ..Default::default()
        };
        let initial = llm_selection_digest(&settings);
        settings.provider_model = Some("different-unversioned-model".into());
        assert_ne!(initial, llm_selection_digest(&settings));

        settings.base_url =
            Some("https://user:first-secret@example.test?token=second-secret".into());
        let unsafe_url_digest = llm_selection_digest(&settings);
        settings.base_url =
            Some("https://other:third-secret@example.test?token=fourth-secret".into());
        assert_eq!(unsafe_url_digest, llm_selection_digest(&settings));
        assert!(!unsafe_url_digest.contains("secret"));
    }

    #[test]
    fn preview_state_gate_accepts_only_known_open_canonical_or_legacy_values() {
        for state in ["OPEN", "IN_PROGRESS", "open", "in_progress"] {
            assert_eq!(coding_agent_issue_state_blocker(state), None, "{state}");
        }
        for state in [
            "AWAITING_REVIEW",
            "DONE",
            "CANCELED",
            "CANCELLED",
            "BLOCKED",
            "DEFERRED",
            "UNKNOWN",
            "OPENED",
            "not_open",
        ] {
            assert_eq!(
                coding_agent_issue_state_blocker(state),
                Some("issue_not_open_for_work"),
                "{state} must not be admitted by general case conversion"
            );
        }
    }
}
