import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  codingAgentApproveAndStart,
  codingAgentAttachRun,
  codingAgentCancelRun,
  codingAgentGetBackendStatus,
  codingAgentGetSettings,
  codingAgentListDelegationPlans,
  codingAgentListRuns,
  codingAgentPreviewRun,
  getLlmSettings,
} from "@/api";
import { Modal } from "@/components/Modal";
import type {
  CodingAgentApprovalAssessmentContract,
  CodingAgentCriterionEvaluationMethod,
  CodingAgentCriterionPolicyRequest,
  CodingAgentRepositoryIdentity,
  CodingAgentRunPreview,
  CodingAgentRunPreviewRequest,
  CodingAgentRunStatus,
  IssueListItem,
} from "@/types/api";
import {
  approvalContractMatchesCanonicalCriteria,
  isCodingAgentApprovalAssessmentContract,
} from "./assessment";
import { CodingAgentEvidenceViewer } from "./CodingAgentEvidenceViewer";
import { CodingAgentReviewPanel } from "./CodingAgentReviewPanel";
import { parseValidationCommands, validationDraftsMatchPreview } from "./validationCommands";

const SETTINGS_QUERY_KEY = ["coding-agent-settings"] as const;
const BACKEND_QUERY_KEY = ["coding-agent-backend-status"] as const;

interface PreviewSnapshot {
  requestKey: string;
  contextKey: string;
  contextGeneration: number;
  request: CodingAgentRunPreviewRequest;
  preview: CodingAgentRunPreview;
}

interface StartOutcome {
  started: boolean;
  runId: string | null;
  blockers: string[];
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function repositoryLabel(repository: CodingAgentRepositoryIdentity): string {
  return `${repository.host}/${repository.organization}/${repository.repository}`;
}

function policiesMatchPreview(
  contract: CodingAgentApprovalAssessmentContract,
  policies: CodingAgentCriterionPolicyRequest[] | undefined,
): boolean {
  return (
    policies === undefined ||
    (policies.length === contract.criteria.length &&
      contract.criteria.every((criterion, index) => {
        const policy = policies[index];
        return (
          policy?.criterionId === criterion.criterion_id &&
          policy.required === criterion.required &&
          policy.method === criterion.method
        );
      }))
  );
}

function criterionMethodLabel(
  method: CodingAgentCriterionEvaluationMethod,
  t: ReturnType<typeof useTranslation>["t"],
  japanese: boolean,
): string {
  const labels = {
    deterministic: ["Deterministic", "決定的評価"],
    semantic_judge: ["Semantic judge", "Semantic judge"],
    unknown: ["Unknown", "不明"],
  } as const;
  const [english, ja] = labels[method];
  return t(`codingAgent.run.assessment.method.${method}`, {
    defaultValue: japanese ? ja : english,
  });
}

function isCurrentAssessmentProviderSupported(contract: CodingAgentApprovalAssessmentContract) {
  return (
    contract.provider !== null &&
    contract.prompt_policy_version === "coding-agent-judge-v1" &&
    contract.input_scope.allowed_fields.includes("report") &&
    contract.input_scope.allowed_sources.filter((source) => source.kind === "report").length === 1
  );
}

function approvedInputFieldLabel(
  field: CodingAgentApprovalAssessmentContract["input_scope"]["allowed_fields"][number],
  t: ReturnType<typeof useTranslation>["t"],
  japanese: boolean,
): string {
  const labels = {
    issue_title: ["Issue title", "Issue title"],
    issue_body: ["Issue body", "Issue body"],
    repository_identity: ["Repository identity", "Repository identity"],
    repository_summary: ["Repository summary", "Repository summary"],
    thread_group_context: ["Thread-group context", "Thread-group context"],
    bounded_file: ["Bounded file", "Bounded file"],
    bounded_diff: ["Bounded diff", "Bounded diff"],
    report: ["Agent report", "Agent report"],
    validation_result: ["Validation result", "Validation result"],
  } as const;
  const [english, ja] = labels[field];
  return t(`codingAgent.run.assessment.field.${field}`, {
    defaultValue: japanese ? ja : english,
  });
}

function parseRepository(value: string): CodingAgentRepositoryIdentity | null {
  const parts = value.split("/").map((part) => part.trim());
  const [host = "", organization = "", repository = "", extra] = parts;
  if (extra !== undefined || !host || !organization || !repository) return null;
  return { host, organization, repository };
}

function isRecoverableState(state: CodingAgentRunStatus["state"]): boolean {
  return [
    "approval_pending",
    "queued",
    "preparing",
    "running",
    "finalizing",
    "verifying",
    "publishing",
    "unknown",
  ].includes(state);
}

function updateRunCache(
  queryClient: ReturnType<typeof useQueryClient>,
  issueId: string,
  run: CodingAgentRunStatus,
) {
  queryClient.setQueryData<CodingAgentRunStatus[]>(["coding-agent-runs", issueId], (current) => {
    if (!current) return [run];
    const found = current.some((entry) => entry.runId === run.runId);
    return found
      ? current.map((entry) => (entry.runId === run.runId ? run : entry))
      : [run, ...current];
  });
}

export function CodingAgentRunPanel({ issue }: { issue: IssueListItem }) {
  const { t, i18n } = useTranslation();
  const japanese = i18n.resolvedLanguage?.startsWith("ja") ?? i18n.language.startsWith("ja");
  const queryClient = useQueryClient();
  const settingsQuery = useQuery({
    queryKey: SETTINGS_QUERY_KEY,
    queryFn: codingAgentGetSettings,
    retry: false,
  });
  const backendQuery = useQuery({
    queryKey: BACKEND_QUERY_KEY,
    queryFn: codingAgentGetBackendStatus,
    retry: false,
  });
  const llmSettingsQuery = useQuery({
    queryKey: ["llm-settings"],
    queryFn: getLlmSettings,
    retry: false,
  });
  const runsQuery = useQuery({
    queryKey: ["coding-agent-runs", issue.id],
    queryFn: codingAgentListRuns,
    retry: false,
    refetchInterval: (query) =>
      query.state.data?.some((run) => isRecoverableState(run.state)) ? 3000 : false,
  });

  const delegationPlansQuery = useQuery({
    queryKey: ["coding-agent-delegation-plans", issue.id],
    queryFn: () => codingAgentListDelegationPlans(issue.id),
    retry: false,
    refetchInterval: 5000,
  });
  const [delegationPlanChoice, setDelegationPlanChoice] = useState<string | null>(null);
  const pendingDelegations = (delegationPlansQuery.data ?? []).filter(
    (plan) => plan.request.issueId === issue.id && plan.runId === null,
  );
  const settings = settingsQuery.data?.settings;
  const profiles = settings?.profiles ?? [];
  const [profileChoice, setProfileChoice] = useState<string | null>(null);
  const profileId = profiles.some((profile) => profile.profileId === profileChoice)
    ? (profileChoice ?? "")
    : settings?.activeProfileId &&
        profiles.some((profile) => profile.profileId === settings.activeProfileId)
      ? settings.activeProfileId
      : (profiles[0]?.profileId ?? "");
  const selectedProfile = profiles.find((profile) => profile.profileId === profileId);
  const mountAvailability = settingsQuery.data?.profileMountAvailability.find(
    (entry) => entry.profileId === profileId,
  );
  const llmSettings = llmSettingsQuery.data;
  const llmSelectionKey = JSON.stringify(llmSettings ?? null);

  const executionMode = "one_shot" as const;
  const [purpose, setPurpose] = useState<CodingAgentRunPreviewRequest["purpose"]>("investigation");
  const [publicationPlan, setPublicationPlan] =
    useState<CodingAgentRunPreviewRequest["publicationPlan"]>("no_publication");
  const [autoDoneRequested, setAutoDoneRequested] = useState(false);
  const [prompt, setPrompt] = useState("");
  const [validationText, setValidationText] = useState("");
  const [validationCwd, setValidationCwd] = useState("/workspace");
  const [validationTimeout, setValidationTimeout] = useState(300);
  const [validationRequired, setValidationRequired] = useState(true);
  const validationCommands = useMemo(
    () =>
      parseValidationCommands(validationText, validationCwd, validationTimeout, validationRequired),
    [validationText, validationCwd, validationTimeout, validationRequired],
  );
  const [criterionPolicies, setCriterionPolicies] = useState<
    CodingAgentCriterionPolicyRequest[] | null
  >(null);
  const [manualRepositoryText, setManualRepositoryText] = useState("");
  const manualRepository = useMemo(
    () => (manualRepositoryText.trim() ? parseRepository(manualRepositoryText) : null),
    [manualRepositoryText],
  );
  const autoDoneAllowed = settings?.autoDoneEnabled === true && settings.semanticJudgeEnabled;
  useEffect(() => {
    if (!autoDoneAllowed) setAutoDoneRequested(false);
  }, [autoDoneAllowed]);
  const request = useMemo<CodingAgentRunPreviewRequest>(() => {
    const next: CodingAgentRunPreviewRequest = {
      issueId: issue.id,
      profileId,
      executionMode,
      purpose,
      publicationPlan,
      autoDoneRequested: autoDoneRequested && autoDoneAllowed,
    };
    if (manualRepository) next.manualRepository = manualRepository;
    if (prompt.length > 0) next.prompt = prompt;
    if (validationCommands?.length) next.validationDrafts = validationCommands;
    if (criterionPolicies !== null) next.criterionPolicies = criterionPolicies;
    return next;
  }, [
    autoDoneAllowed,
    autoDoneRequested,
    criterionPolicies,
    validationCommands,
    executionMode,
    issue.id,
    manualRepository,
    profileId,
    prompt,
    publicationPlan,
    purpose,
  ]);
  const selectedDelegation = pendingDelegations.find(
    (plan) => plan.delegationPlanId === delegationPlanChoice,
  );
  const delegationPlanId =
    selectedDelegation &&
    selectedDelegation.request.profileId === profileId &&
    selectedDelegation.request.purpose === purpose &&
    (selectedDelegation.request.prompt ?? "") === prompt
      ? selectedDelegation.delegationPlanId
      : null;
  const requestKey = JSON.stringify([
    request,
    validationCommands === null ? validationText : null,
    delegationPlanId,
  ]);
  const issueCriteriaKey = JSON.stringify([
    issue.id,
    issue.revision,
    issue.acceptanceCriteriaItems ?? null,
  ]);
  const issueContextKey = JSON.stringify(issue);
  const contextKey = JSON.stringify([
    requestKey,
    settings?.revision ?? null,
    selectedProfile?.version ?? null,
    llmSelectionKey,
    issueContextKey,
  ]);
  const previewContextGeneration = useRef({ contextKey, generation: 0 });

  const [previewSnapshot, setPreviewSnapshot] = useState<PreviewSnapshot | null>(null);
  const [previewing, setPreviewing] = useState(false);
  const [approving, setApproving] = useState(false);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [startOutcome, setStartOutcome] = useState<StartOutcome | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [userConfirmed, setUserConfirmed] = useState(false);
  const [manualRepositoryConfirmed, setManualRepositoryConfirmed] = useState(false);
  const [autoDoneConfirmed, setAutoDoneConfirmed] = useState(false);
  const [selectedEvidenceRunId, setSelectedEvidenceRunId] = useState<string | null>(null);
  const [selectedReviewRunId, setSelectedReviewRunId] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const previewRequestId = useRef(0);
  const assessmentText = (key: string, english: string, japaneseText: string) =>
    t(`codingAgent.run.assessment.${key}`, {
      defaultValue: japanese ? japaneseText : english,
    });

  useLayoutEffect(() => {
    const previousContext = previewContextGeneration.current;
    if (previousContext.contextKey === contextKey) return;
    previewContextGeneration.current = {
      contextKey,
      generation: previousContext.generation + 1,
    };
    previewRequestId.current += 1;
  }, [contextKey]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: reset pending approval state when the derived context changes.
  useEffect(() => {
    previewRequestId.current += 1;
    setPreviewing(false);
    setConfirming(false);
    setUserConfirmed(false);
    setManualRepositoryConfirmed(false);
    setAutoDoneConfirmed(false);
  }, [contextKey]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: clear criterion policy and preview state when canonical criteria change.
  useEffect(() => {
    setCriterionPolicies(null);
    setPreviewSnapshot(null);
  }, [issueCriteriaKey]);

  const currentPreview = previewSnapshot?.preview ?? null;
  const assessmentContract =
    currentPreview &&
    isCodingAgentApprovalAssessmentContract(
      currentPreview.assessmentContract,
      currentPreview.assessmentContractDigest,
      currentPreview.assessmentProviderSupported,
    ) &&
    currentPreview.assessmentContract.purpose === currentPreview.purpose
      ? currentPreview.assessmentContract
      : null;
  const previewMatchesRequest = Boolean(
    previewSnapshot &&
      previewSnapshot.contextGeneration === previewContextGeneration.current.generation &&
      previewContextGeneration.current.contextKey === contextKey &&
      previewSnapshot.requestKey === requestKey &&
      previewSnapshot.contextKey === contextKey &&
      currentPreview?.issueId === issue.id &&
      Number.isSafeInteger(Number(issue.revision)) &&
      currentPreview?.issueRevision === Number(issue.revision) &&
      currentPreview?.settingsRevision === settings?.revision &&
      currentPreview?.profileId === profileId &&
      currentPreview?.profileVersion === (selectedProfile?.version ?? null) &&
      currentPreview?.executionMode === request.executionMode &&
      currentPreview?.purpose === request.purpose &&
      currentPreview?.publicationPlan === request.publicationPlan &&
      currentPreview?.autoDoneRequested === request.autoDoneRequested,
  );
  const canonicalCriteriaMatch = Boolean(
    assessmentContract &&
      currentPreview &&
      approvalContractMatchesCanonicalCriteria(assessmentContract, issue.acceptanceCriteriaItems),
  );
  const requestPoliciesMatchContract = Boolean(
    assessmentContract && policiesMatchPreview(assessmentContract, request.criterionPolicies),
  );
  const requestValidationsMatchContract = Boolean(
    assessmentContract &&
      validationDraftsMatchPreview(assessmentContract, request.validationDrafts),
  );
  const contractMatchesIssue =
    canonicalCriteriaMatch && requestPoliciesMatchContract && requestValidationsMatchContract;
  const providerSelectionMatchesCurrent = Boolean(
    currentPreview &&
      assessmentContract &&
      (assessmentContract.provider === null
        ? currentPreview.assessmentProviderSupported === false
        : assessmentContract.provider.provider_id === "local"
          ? llmSettings?.mode === "local"
          : llmSettings?.mode === "external" &&
            llmSettings.provider_model === assessmentContract.provider.model_id),
  );
  const contractCanonicalMatchesIssue = Boolean(
    assessmentContract && currentPreview && canonicalCriteriaMatch,
  );
  const assessmentReady = Boolean(
    previewMatchesRequest && contractMatchesIssue && providerSelectionMatchesCurrent,
  );
  const canonicalPlanComplete = Boolean(
    currentPreview?.baseCommit &&
      /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(currentPreview.baseCommit) &&
      currentPreview.workspaceBranch &&
      currentPreview.imageDigest &&
      /^sha256:[0-9a-f]{64}$/.test(currentPreview.imageDigest) &&
      assessmentContract,
  );
  const canApprove = Boolean(
    validationCommands !== null &&
      previewMatchesRequest &&
      assessmentReady &&
      canonicalPlanComplete &&
      currentPreview?.canApprove &&
      currentPreview.blockers.length === 0 &&
      !previewing &&
      !approving,
  );
  const matchingRuns = (runsQuery.data ?? []).filter((run) => run.issueId === issue.id);
  const selectedEvidenceRun =
    matchingRuns.find((run) => run.runId === selectedEvidenceRunId) ?? null;
  const selectedReviewRun = matchingRuns.find((run) => run.runId === selectedReviewRunId) ?? null;
  const semanticJudgeSupported = Boolean(
    assessmentContract &&
      currentPreview?.assessmentProviderSupported === true &&
      providerSelectionMatchesCurrent &&
      isCurrentAssessmentProviderSupported(assessmentContract),
  );
  const selectedLlmSettings = llmSettings;
  const selectedModelDiagnostic =
    selectedLlmSettings?.provider_model ??
    selectedLlmSettings?.local_preset_id ??
    selectedLlmSettings?.local_model_file ??
    selectedLlmSettings?.local_hf_repo ??
    null;
  const policyOverrideMatchesContract = Boolean(
    assessmentContract && policiesMatchPreview(assessmentContract, request.criterionPolicies),
  );

  const criterionPolicyFor = (criterionId: string) => {
    const nativeCriterion = assessmentContract?.criteria.find(
      (criterion) => criterion.criterion_id === criterionId,
    );
    const requestedPolicy = request.criterionPolicies?.find(
      (policy) => policy.criterionId === criterionId,
    );
    if (previewMatchesRequest && !policyOverrideMatchesContract) {
      if (nativeCriterion) {
        return {
          criterionId,
          required: nativeCriterion.required,
          method: nativeCriterion.method,
        } satisfies CodingAgentCriterionPolicyRequest;
      }
    }
    if (requestedPolicy) return requestedPolicy;
    if (nativeCriterion) {
      return {
        criterionId,
        required: nativeCriterion.required,
        method: nativeCriterion.method,
      } satisfies CodingAgentCriterionPolicyRequest;
    }
    return null;
  };

  const updateCriterionPolicy = (
    criterionId: string,
    update: Partial<Pick<CodingAgentCriterionPolicyRequest, "required" | "method">>,
  ) => {
    if (!assessmentContract || !contractCanonicalMatchesIssue) return;
    if (update.method === "semantic_judge" && !semanticJudgeSupported) return;
    const currentPolicies = assessmentContract.criteria.map((criterion) => {
      const existing = request.criterionPolicies?.find(
        (policy) => policy.criterionId === criterion.criterion_id,
      );
      return {
        criterionId: criterion.criterion_id,
        required: existing?.required ?? criterion.required,
        method: existing?.method ?? criterion.method,
      };
    });
    setCriterionPolicies(
      currentPolicies.map((policy) =>
        policy.criterionId === criterionId ? { ...policy, ...update } : policy,
      ),
    );
  };

  useEffect(() => {
    if (profiles.length === 0) {
      setProfileChoice(null);
      return;
    }
    if (profileChoice && profiles.some((profile) => profile.profileId === profileChoice)) return;
    setProfileChoice(settings?.activeProfileId ?? profiles[0]?.profileId ?? null);
  }, [profileChoice, profiles, settings?.activeProfileId]);

  const refreshRuns = async () => {
    await runsQuery.refetch();
  };

  const previewRun = async () => {
    if (
      !profileId ||
      validationCommands === null ||
      previewing ||
      approving ||
      (manualRepositoryText.trim() && !manualRepository)
    ) {
      return;
    }
    const contextGeneration = previewContextGeneration.current.generation;
    const generationContextKey = previewContextGeneration.current.contextKey;
    const requestId = ++previewRequestId.current;
    const previewRequestKey = requestKey;
    const previewContextKey = contextKey;
    setPreviewing(true);
    setPreviewError(null);
    setStartOutcome(null);
    setConfirming(false);
    try {
      const result = await codingAgentPreviewRun(request);
      if (
        requestId !== previewRequestId.current ||
        contextGeneration !== previewContextGeneration.current.generation ||
        generationContextKey !== previewContextGeneration.current.contextKey
      ) {
        return;
      }
      setPreviewSnapshot({
        requestKey: previewRequestKey,
        contextKey: previewContextKey,
        contextGeneration,
        request,
        preview: result,
      });
    } catch (error) {
      if (requestId !== previewRequestId.current) return;
      setPreviewError(errorText(error));
      setPreviewSnapshot(null);
    } finally {
      if (requestId === previewRequestId.current) setPreviewing(false);
    }
  };

  const openApproval = () => {
    if (!canApprove || !previewSnapshot) return;
    setUserConfirmed(false);
    setManualRepositoryConfirmed(false);
    setAutoDoneConfirmed(false);
    setPreviewError(null);
    setConfirming(true);
  };

  const approveAndStart = async () => {
    if (!previewSnapshot || !canApprove || !userConfirmed || approving) return;
    const previewRequest = previewSnapshot.request;
    setApproving(true);
    setPreviewError(null);
    setStartOutcome(null);
    try {
      const result = await codingAgentApproveAndStart({
        preview: previewRequest,
        ...(delegationPlanId ? { delegationPlanId } : {}),
        expectedPlanId: previewSnapshot.preview.planId,
        expectedDigest: previewSnapshot.preview.digest,
        userConfirmed: true,
        manualRepositoryConfirmed: previewRequest.manualRepository
          ? manualRepositoryConfirmed
          : false,
        autoDoneConfirmed: previewRequest.autoDoneRequested ? autoDoneConfirmed : false,
      });
      setConfirming(false);
      if (!result.started || !result.runId) {
        setStartOutcome({
          started: false,
          runId: null,
          blockers: result.blockers.length > 0 ? result.blockers : ["start_not_confirmed"],
        });
        return;
      }
      setStartOutcome({ started: true, runId: result.runId, blockers: result.blockers });
      await queryClient.invalidateQueries({ queryKey: ["coding-agent-runs", issue.id] });
      await refreshRuns();
    } catch (error) {
      setConfirming(false);
      setPreviewError(errorText(error));
      // A CAS/digest failure invalidates this approval snapshot. Refresh is an explicit user action.
      setPreviewSnapshot(null);
    } finally {
      setApproving(false);
    }
  };

  const cancelRun = async (runId: string) => {
    setActionError(null);
    try {
      const owner = await codingAgentAttachRun(runId);
      if (owner.runId !== runId) throw new Error("run ownership response did not match request");
      const response = await codingAgentCancelRun(owner.runId);
      updateRunCache(queryClient, issue.id, response.run);
      await refreshRuns();
    } catch (error) {
      const message = errorText(error);
      setActionError(
        /owned_by_another_window|another application window|owned by another window/i.test(message)
          ? t("codingAgent.run.ownerUnavailable")
          : t("codingAgent.run.runActionUnavailable"),
      );
    }
  };

  const hasInvalidManualRepository = Boolean(manualRepositoryText.trim() && !manualRepository);
  const confirmationAssessmentContract =
    previewSnapshot &&
    isCodingAgentApprovalAssessmentContract(
      previewSnapshot.preview.assessmentContract,
      previewSnapshot.preview.assessmentContractDigest,
      previewSnapshot.preview.assessmentProviderSupported,
    ) &&
    previewSnapshot.preview.assessmentContract.purpose === previewSnapshot.preview.purpose
      ? previewSnapshot.preview.assessmentContract
      : null;

  return (
    <section
      aria-label={t("codingAgent.run.title")}
      className="settings-card coding-agent-run-panel"
      style={{ display: "grid", gap: 10 }}
    >
      <div>
        <div className="settings-card-title">{t("codingAgent.run.title")}</div>
        <div className="settings-card-desc">{t("codingAgent.run.description")}</div>
      </div>

      {backendQuery.isError ? (
        <p role="status" className="settings-card-desc">
          {t("codingAgent.run.backendUnavailable")}
        </p>
      ) : backendQuery.data?.enabled ? (
        <p role="status" className="settings-card-desc">
          {t("codingAgent.run.backendEnabled")}
        </p>
      ) : (
        <div role="status" className="settings-card-desc">
          {t("codingAgent.run.backendDisabled")}
          {backendQuery.data?.blockers.map((blocker) => (
            <div key={blocker}>
              <code>{blocker}</code>
            </div>
          ))}
        </div>
      )}

      {settingsQuery.isError && (
        <p role="alert" className="issues-error">
          {t("codingAgent.run.settingsUnavailable")} {errorText(settingsQuery.error)}
        </p>
      )}
      {!settingsQuery.isError && profiles.length === 0 && (
        <p role="status" className="settings-card-desc">
          {t("codingAgent.run.noProfile")}
        </p>
      )}
      {selectedProfile && mountAvailability && !mountAvailability.available && (
        <div role="status" className="settings-card-desc">
          {t("codingAgent.run.profileMountUnavailable")}
          {mountAvailability.blockers.map((blocker) => (
            <code key={blocker}> {blocker}</code>
          ))}
        </div>
      )}

      {delegationPlansQuery.isError && (
        <p role="alert">{t("codingAgent.run.delegationLoadError")}</p>
      )}
      {pendingDelegations.length > 0 && (
        <section aria-label={t("codingAgent.run.delegationPlans")}>
          <p>{t("codingAgent.run.delegationApproval")}</p>
          {pendingDelegations.map((plan) => (
            <div key={plan.delegationPlanId}>
              <span>
                {plan.delegationPlanId} · {plan.request.profileId} · {plan.request.purpose}
              </span>
              <button
                type="button"
                disabled={!profiles.some((profile) => profile.profileId === plan.request.profileId)}
                onClick={() => {
                  setProfileChoice(plan.request.profileId);
                  setPurpose(plan.request.purpose);
                  setPrompt(plan.request.prompt ?? "");
                  setDelegationPlanChoice(plan.delegationPlanId);
                  setCriterionPolicies(null);
                  setAutoDoneRequested(false);
                }}
              >
                {t("codingAgent.run.loadDelegation")}
              </button>
            </div>
          ))}
        </section>
      )}
      <div className="settings-row">
        <label className="settings-row-label" htmlFor={`coding-agent-run-profile-${issue.id}`}>
          {t("codingAgent.run.profile")}
        </label>
        <select
          id={`coding-agent-run-profile-${issue.id}`}
          value={profileId}
          onChange={(event) => setProfileChoice(event.currentTarget.value || null)}
          disabled={profiles.length === 0}
        >
          {profiles.map((profile) => (
            <option key={profile.profileId} value={profile.profileId}>
              {profile.displayName || profile.profileId}
            </option>
          ))}
        </select>
      </div>
      <div className="settings-row">
        <label className="settings-row-label" htmlFor={`coding-agent-run-mode-${issue.id}`}>
          {t("codingAgent.run.mode")}
        </label>
        <select id={`coding-agent-run-mode-${issue.id}`} value={executionMode} disabled>
          <option value="one_shot">{t("codingAgent.run.oneShot")}</option>
        </select>
      </div>
      <div className="settings-row">
        <label className="settings-row-label" htmlFor={`coding-agent-run-purpose-${issue.id}`}>
          {t("codingAgent.run.purpose")}
        </label>
        <select
          id={`coding-agent-run-purpose-${issue.id}`}
          value={purpose}
          onChange={(event) => {
            const nextPurpose = event.currentTarget
              .value as CodingAgentRunPreviewRequest["purpose"];
            setPurpose(nextPurpose);
            if (nextPurpose !== "implementation") setAutoDoneRequested(false);
          }}
        >
          <option value="investigation">{t("codingAgent.run.investigation")}</option>
          <option value="implementation">{t("codingAgent.run.implementation")}</option>
        </select>
      </div>
      <div className="settings-row">
        <label className="settings-row-label" htmlFor={`coding-agent-run-publication-${issue.id}`}>
          {t("codingAgent.run.publication")}
        </label>
        <select
          id={`coding-agent-run-publication-${issue.id}`}
          value={publicationPlan}
          onChange={(event) =>
            setPublicationPlan(
              event.currentTarget.value as CodingAgentRunPreviewRequest["publicationPlan"],
            )
          }
        >
          <option value="no_publication">{t("codingAgent.run.noPublication")}</option>
          <option value="push_branch">{t("codingAgent.run.pushBranch")}</option>
          <option value="create_pull_request">{t("codingAgent.run.createPullRequest")}</option>
        </select>
      </div>
      <label className="settings-row">
        <span className="settings-row-label">{t("codingAgent.run.manualRepository")}</span>
        <input
          aria-label={t("codingAgent.run.manualRepository")}
          value={manualRepositoryText}
          onChange={(event) => setManualRepositoryText(event.currentTarget.value)}
          placeholder={t("codingAgent.run.manualRepositoryPlaceholder")}
        />
      </label>
      {hasInvalidManualRepository && (
        <p className="issues-error">{t("codingAgent.run.invalidManualRepository")}</p>
      )}
      <label className="settings-row">
        <span className="settings-row-label">{t("codingAgent.run.prompt")}</span>
        <textarea
          aria-label={t("codingAgent.run.prompt")}
          value={prompt}
          onChange={(event) => setPrompt(event.currentTarget.value)}
          rows={3}
        />
      </label>
      <label className="settings-row">
        <span className="settings-row-label">{t("codingAgent.run.autoDone")}</span>
        <input
          type="checkbox"
          aria-label={t("codingAgent.run.autoDone")}
          checked={autoDoneRequested}
          disabled={!autoDoneAllowed}
          onChange={(event) => setAutoDoneRequested(event.currentTarget.checked)}
        />
        <span className="settings-card-desc">{t("codingAgent.run.autoDoneNote")}</span>
      </label>

      <fieldset>
        <legend>{t("codingAgent.run.validationCommands")}</legend>
        <textarea
          aria-label={t("codingAgent.run.validationCommands")}
          value={validationText}
          rows={3}
          placeholder={'["cargo", "test"]'}
          onChange={(event) => setValidationText(event.currentTarget.value)}
        />
        <label className="settings-row">
          <span>{t("codingAgent.run.validationCwd")}</span>
          <input
            aria-label={t("codingAgent.run.validationCwd")}
            value={validationCwd}
            onChange={(event) => setValidationCwd(event.currentTarget.value)}
          />
        </label>
        <label className="settings-row">
          <span>{t("codingAgent.run.validationTimeout")}</span>
          <input
            type="number"
            min={1}
            max={10800}
            step={1}
            aria-label={t("codingAgent.run.validationTimeout")}
            value={validationTimeout}
            onChange={(event) => setValidationTimeout(event.currentTarget.valueAsNumber)}
          />
        </label>
        <label className="settings-row">
          <span>{t("codingAgent.run.validationRequired")}</span>
          <input
            type="checkbox"
            aria-label={t("codingAgent.run.validationRequired")}
            checked={validationRequired}
            onChange={(event) => setValidationRequired(event.currentTarget.checked)}
          />
        </label>
        {validationCommands === null && (
          <p role="alert">{t("codingAgent.run.validationInvalid")}</p>
        )}
      </fieldset>
      {previewError && (
        <p role="alert" className="issues-error">
          {previewError}
        </p>
      )}
      <div className="settings-row">
        <button
          type="button"
          className="btn"
          disabled={
            !profileId ||
            previewing ||
            approving ||
            hasInvalidManualRepository ||
            validationCommands === null ||
            settingsQuery.isPending ||
            llmSettingsQuery.isPending
          }
          onClick={() => void previewRun()}
        >
          {previewing
            ? t("codingAgent.run.previewing")
            : currentPreview && !previewMatchesRequest
              ? t("codingAgent.run.refreshPlan")
              : t("codingAgent.run.preview")}
        </button>
        {currentPreview && previewMatchesRequest && (
          <button
            type="button"
            className="btn primary"
            disabled={!canApprove || approving}
            onClick={openApproval}
          >
            {t("codingAgent.run.approve")}
          </button>
        )}
      </div>

      {currentPreview && (
        <section
          className="coding-agent-run-preview"
          aria-label={t("codingAgent.run.previewFacts")}
        >
          <h3>{t("codingAgent.run.previewFacts")}</h3>
          <dl className="issues-definition">
            <dt>{t("codingAgent.run.planId")}</dt>
            <dd>
              <code>{currentPreview.planId}</code>
            </dd>
            <dt>{t("codingAgent.run.digest")}</dt>
            <dd>
              <code>{currentPreview.digest}</code>
            </dd>
            <dt>{t("codingAgent.run.issue")}</dt>
            <dd>{currentPreview.issueId}</dd>
            <dt>{t("codingAgent.run.issueRevision")}</dt>
            <dd>{currentPreview.issueRevision}</dd>
            <dt>{t("codingAgent.run.issueState")}</dt>
            <dd>{currentPreview.issueState}</dd>
            <dt>{t("codingAgent.run.issueTitle")}</dt>
            <dd>{currentPreview.issueTitle}</dd>
            <dt>{t("codingAgent.run.settingsRevision")}</dt>
            <dd>{currentPreview.settingsRevision}</dd>
            <dt>{t("codingAgent.run.profileIdentity")}</dt>
            <dd>
              {currentPreview.profileId ?? t("codingAgent.run.unavailable")}
              {currentPreview.profileVersion !== null
                ? ` · ${t("codingAgent.run.profileVersion", { version: currentPreview.profileVersion })}`
                : ""}
            </dd>
            <dt>{t("codingAgent.run.mode")}</dt>
            <dd>
              {t(
                `codingAgent.run.${
                  currentPreview.executionMode === "interactive" ? "interactive" : "oneShot"
                }`,
              )}
            </dd>
            <dt>{t("codingAgent.run.purpose")}</dt>
            <dd>
              {t(
                `codingAgent.run.${
                  currentPreview.purpose === "implementation" ? "implementation" : "investigation"
                }`,
              )}
            </dd>
            <dt>{t("codingAgent.run.publication")}</dt>
            <dd>
              {t(
                `codingAgent.run.${
                  currentPreview.publicationPlan === "create_pull_request"
                    ? "createPullRequest"
                    : currentPreview.publicationPlan === "push_branch"
                      ? "pushBranch"
                      : "noPublication"
                }`,
              )}
            </dd>
            <dt>{t("codingAgent.run.autoDone")}</dt>
            <dd>
              {currentPreview.autoDoneRequested
                ? t("codingAgent.run.autoDoneOn")
                : t("codingAgent.run.autoDoneOff")}
            </dd>
            <dt>{t("codingAgent.run.selectedRepository")}</dt>
            <dd>
              {currentPreview.selectedRepository
                ? repositoryLabel(currentPreview.selectedRepository)
                : (currentPreview.repositorySelectionReason ??
                  t("codingAgent.run.repositoryUnresolved"))}
            </dd>
            {currentPreview.baseCommit && (
              <>
                <dt>{assessmentText("baseCommit", "Base commit", "開始元commit")}</dt>
                <dd>
                  <code>{currentPreview.baseCommit}</code>
                </dd>
              </>
            )}
            {currentPreview.workspaceBranch && (
              <>
                <dt>{assessmentText("workspaceBranch", "Issue branch", "Issue branch")}</dt>
                <dd>
                  <code>{currentPreview.workspaceBranch}</code>
                </dd>
              </>
            )}
            {currentPreview.imageDigest && (
              <>
                <dt>{assessmentText("imageDigest", "Image digest", "Image digest")}</dt>
                <dd>
                  <code>{currentPreview.imageDigest}</code>
                </dd>
              </>
            )}
          </dl>
          <section
            aria-label={assessmentText("title", "Approval assessment contract", "承認時の評価契約")}
            className="coding-agent-assessment-contract"
          >
            <h4>{assessmentText("title", "Approval assessment contract", "承認時の評価契約")}</h4>
            {!Array.isArray(issue.acceptanceCriteriaItems) && (
              <p role="status" className="issues-error">
                {assessmentText(
                  "canonicalMissing",
                  "Canonical acceptance criteria are unavailable on this legacy Issue. The joined completion text is not used as a fallback.",
                  "Legacy Issue に canonical acceptance criteria vector がありません。結合済み completion text からは補完しません。",
                )}
              </p>
            )}
            {Array.isArray(issue.acceptanceCriteriaItems) &&
              issue.acceptanceCriteriaItems.length === 0 && (
                <p className="settings-card-desc">
                  {assessmentText(
                    "wholeIssueAssessment",
                    "This Issue has no acceptance criteria. Semantic judging evaluates the whole Issue request.",
                    "この Issue には受入条件がありません。意味判定では Issue の依頼全体が解決したかを評価します。",
                  )}
                </p>
              )}
            {!assessmentContract ? (
              <p role="status" className="issues-error">
                {assessmentText(
                  "contractUnavailable",
                  "The native approval assessment contract is missing or invalid. Approval is blocked.",
                  "Native approval assessment contract がないか無効です。承認を block します。",
                )}
              </p>
            ) : (
              <>
                <p className="settings-card-desc">
                  {assessmentText(
                    "contractDigest",
                    "Assessment contract digest",
                    "評価契約 digest",
                  )}
                  : <code>{currentPreview.assessmentContractDigest}</code>
                </p>
                {!canonicalCriteriaMatch && Array.isArray(issue.acceptanceCriteriaItems) && (
                  <p role="status" className="issues-error">
                    {assessmentText(
                      "canonicalMismatch",
                      "Native criteria do not match the current canonical Issue vector; refresh the Issue before approval.",
                      "Native criteria と現在の canonical Issue vector が一致しません。承認前に Issue を再読込してください。",
                    )}
                  </p>
                )}
                {canonicalCriteriaMatch && !requestPoliciesMatchContract && (
                  <p role="status" className="issues-error">
                    {assessmentText(
                      "policyMismatch",
                      "The native preview did not apply every selected criterion policy. Approval is blocked until a matching preview is returned.",
                      "Native preview が選択したすべての criterion policy を反映していません。一致する preview が返るまで承認できません。",
                    )}
                  </p>
                )}
                {!requestValidationsMatchContract && (
                  <p role="status" className="issues-error">
                    {t("codingAgent.run.validationMismatch")}
                  </p>
                )}
                <h5>
                  {assessmentText(
                    "criteria",
                    "Canonical acceptance criteria",
                    "Canonical acceptance criteria",
                  )}
                </h5>
                <ol>
                  {assessmentContract.criteria.map((criterion) => {
                    const policy = criterionPolicyFor(criterion.criterion_id) ?? {
                      criterionId: criterion.criterion_id,
                      required: criterion.required,
                      method: criterion.method,
                    };
                    const semanticSupported = semanticJudgeSupported;
                    const methodDescription =
                      policy.method === "unknown"
                        ? assessmentText(
                            "unknownExplanation",
                            "Unknown remains unresolved and is not silently skipped; required Unknown criteria block completion.",
                            "Unknown は未解決のまま保持され、暗黙に skip しません。必須 Unknown criterion は完了を block します。",
                          )
                        : policy.method === "deterministic"
                          ? assessmentText(
                              "deterministicExplanation",
                              "Deterministic describes the approved method, not proof. Without an independent host validation result, DONE remains held.",
                              "Deterministic は承認した方法であり証明ではありません。独立した host validation result がなければ DONE は保留されます。",
                            )
                          : assessmentText(
                              "semanticExplanation",
                              "Semantic evaluation uses only the listed approval-time input scope and the exact supported provider selection.",
                              "Semantic evaluation は表示された承認時 input scope と、正確に選択された supported provider だけを使います。",
                            );
                    return (
                      <li key={criterion.criterion_id} style={{ marginBottom: 12 }}>
                        <div>
                          <strong>{criterion.criterion_id}</strong>
                          {" · "}
                          {policy.required
                            ? assessmentText("required", "Required", "必須")
                            : assessmentText("optional", "Optional", "任意")}
                          {" · "}
                          {assessmentText("methodLabel", "Method", "方法")}:{" "}
                          {criterionMethodLabel(policy.method, t, japanese)}
                        </div>
                        <p style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>
                          {criterion.statement}
                        </p>
                        <label>
                          {criterion.criterion_id}{" "}
                          {assessmentText("methodLabel", "evaluation method", "評価方法")}{" "}
                          <select
                            aria-label={`${criterion.criterion_id} ${assessmentText("methodLabel", "evaluation method", "評価方法")}`}
                            value={policy.method}
                            disabled={!contractCanonicalMatchesIssue}
                            onChange={(event) => {
                              const method = event.currentTarget.value;
                              if (
                                method === "deterministic" ||
                                method === "semantic_judge" ||
                                method === "unknown"
                              ) {
                                updateCriterionPolicy(criterion.criterion_id, { method });
                              }
                            }}
                          >
                            <option value="deterministic">
                              {criterionMethodLabel("deterministic", t, japanese)}
                            </option>
                            <option value="semantic_judge" disabled={!semanticSupported}>
                              {criterionMethodLabel("semantic_judge", t, japanese)}
                              {!semanticSupported
                                ? ` · ${assessmentText("unavailable", "unavailable", "利用不可")}`
                                : ""}
                            </option>
                            <option value="unknown">
                              {criterionMethodLabel("unknown", t, japanese)}
                            </option>
                          </select>
                        </label>
                        <label style={{ display: "flex", gap: 6, alignItems: "center" }}>
                          <input
                            type="checkbox"
                            aria-label={`${criterion.criterion_id} ${assessmentText("required", "Required", "必須")}`}
                            checked={policy.required}
                            disabled={!contractCanonicalMatchesIssue}
                            onChange={(event) =>
                              updateCriterionPolicy(criterion.criterion_id, {
                                required: event.currentTarget.checked,
                              })
                            }
                          />
                          {assessmentText("required", "Required", "必須")}
                        </label>
                        <p className="settings-card-desc">{methodDescription}</p>
                      </li>
                    );
                  })}
                </ol>
                <h5>
                  {assessmentText(
                    "validationPlan",
                    "Native validation plan",
                    "Native validation plan",
                  )}
                </h5>
                {assessmentContract.validation_plan.length === 0 ? (
                  <p role="status" className="settings-card-desc">
                    {assessmentText(
                      "noValidations",
                      "No validation definitions were returned. An absent validation is not a passed check.",
                      "Validation definition は返されていません。Validation がないことは check 成功ではありません。",
                    )}
                  </p>
                ) : (
                  <ol>
                    {assessmentContract.validation_plan.map((validation) => (
                      <li key={validation.validation_id} style={{ marginBottom: 8 }}>
                        <strong>{validation.validation_id}</strong>
                        {" · "}
                        {validation.required
                          ? assessmentText("required", "Required", "必須")
                          : assessmentText("optional", "Optional", "任意")}
                        {validation.command === null ? (
                          <p className="settings-card-desc">
                            {assessmentText(
                              "validationCommandMissing",
                              "Validation command is unavailable. None is not PASS.",
                              "Validation command は利用できません。None は PASS ではありません。",
                            )}
                            {validation.required &&
                              ` ${assessmentText("requiredValidationBlocks", "A required validation without a host result keeps DONE blocked.", "必須 validation に host result がなければ DONE は block されます。")}`}
                          </p>
                        ) : (
                          <dl className="issues-definition">
                            <dt>argv</dt>
                            <dd>
                              {validation.command.argv.map((argument, index) => (
                                // biome-ignore lint/suspicious/noArrayIndexKey: argv has no stable identity and repeated arguments are valid.
                                <code key={`${index}:${argument}`} style={{ display: "block" }}>
                                  {argument === "" || /\s/.test(argument)
                                    ? JSON.stringify(argument)
                                    : argument}
                                </code>
                              ))}
                            </dd>
                            <dt>cwd</dt>
                            <dd>
                              <code>{validation.command.working_directory}</code>
                            </dd>
                            <dt>{assessmentText("permissions", "Permissions", "権限")}</dt>
                            <dd>{validation.command.permissions.join(", ")}</dd>
                            <dt>
                              {assessmentText(
                                "profileDigest",
                                "Permission profile digest",
                                "権限 profile digest",
                              )}
                            </dt>
                            <dd>
                              <code>{validation.command.profile_digest}</code>
                            </dd>
                            <dt>{assessmentText("timeout", "Timeout (ms)", "Timeout (ms)")}</dt>
                            <dd>{validation.command.timeout_ms}</dd>
                          </dl>
                        )}
                      </li>
                    ))}
                  </ol>
                )}
                <h5>
                  {assessmentText("provider", "Semantic judge provider", "Semantic judge provider")}
                </h5>
                {assessmentContract.provider && currentPreview.assessmentProviderSupported ? (
                  <>
                    <dl className="issues-definition">
                      <dt>Provider ID</dt>
                      <dd>
                        <code>{assessmentContract.provider.provider_id}</code>
                      </dd>
                      <dt>Model ID</dt>
                      <dd>
                        <code>{assessmentContract.provider.model_id}</code>
                      </dd>
                      <dt>{assessmentText("modelVersion", "Model version", "Model version")}</dt>
                      <dd>
                        <code>{assessmentContract.provider.model_version}</code>
                      </dd>
                      <dt>Config version</dt>
                      <dd>
                        <code>{assessmentContract.provider.config_version}</code>
                      </dd>
                      <dt>
                        {assessmentText(
                          "promptPolicy",
                          "Prompt policy version",
                          "Prompt policy version",
                        )}
                      </dt>
                      <dd>
                        <code>{assessmentContract.prompt_policy_version}</code>
                      </dd>
                    </dl>
                    <p className="settings-card-desc">
                      {assessmentText(
                        "configuredJudgeWorker",
                        "The judge uses the configured Local or External worker. Config version binds app-side settings, not provider-side model weights.",
                        "意味判定には設定済みの Local または External worker を使用します。Config version は app 側設定を束縛し、provider 側 model weights の版を保証しません。",
                      )}
                    </p>
                  </>
                ) : (
                  <div role="status" className="settings-card-desc">
                    <p>
                      {assessmentText(
                        "providerUnavailable",
                        "No supported provider identity was returned. The native judge adapter does not upgrade or switch the selected provider.",
                        "Supported provider identity は返されていません。Native judge adapter は選択中 provider を置換・昇格しません。",
                      )}
                    </p>
                    <p>
                      {assessmentText(
                        "selectedModel",
                        "Selected model setting",
                        "選択中 model 設定",
                      )}
                      :{" "}
                      {selectedModelDiagnostic ??
                        assessmentText("modelUnavailable", "unavailable", "取得できません")}
                    </p>
                    <p>
                      {assessmentText(
                        "judgeWorkerUnavailable",
                        "Check the configured worker readiness and LLM settings before requesting a new preview.",
                        "設定済み worker の準備状態と LLM 設定を確認してください。その後、preview を再取得してください。",
                      )}
                    </p>
                    {llmSettingsQuery.isError && (
                      <p role="status">
                        {assessmentText(
                          "modelDiagnosticUnavailable",
                          "The selected model diagnostic could not be loaded.",
                          "選択中 model の診断を取得できませんでした。",
                        )}
                      </p>
                    )}
                  </div>
                )}
                <h5>
                  {assessmentText("inputScope", "Approved input scope", "承認時 input scope")}
                </h5>
                <p className="settings-card-desc">
                  {assessmentText(
                    "scopeDigestMeaning",
                    "This digest binds approval-time allowed inputs only; it is not a digest of any future assessment payload.",
                    "この digest は承認時の許可 input 範囲だけを束縛します。将来の assessment payload digest ではありません。",
                  )}
                </p>
                <p>
                  {assessmentText(
                    "scopeDigest",
                    "Approval input-scope digest",
                    "承認時 input-scope digest",
                  )}
                  : <code>{assessmentContract.judge_input_scope_digest}</code>
                </p>
                <div>
                  {assessmentText("allowedFields", "Allowed fields", "許可 field")}
                  <ul>
                    {assessmentContract.input_scope.allowed_fields.map((field) => (
                      <li key={field}>{approvedInputFieldLabel(field, t, japanese)}</li>
                    ))}
                  </ul>
                </div>
                <div>
                  {assessmentText(
                    "allowedSources",
                    "Bounded source identities",
                    "上限付き source identity",
                  )}
                  <ul>
                    {assessmentContract.input_scope.allowed_sources.map((source) => (
                      <li key={`${source.kind}:${source.identity}`}>
                        {source.kind} · <code>{source.identity}</code> · {source.max_bytes} bytes
                      </li>
                    ))}
                  </ul>
                </div>
                <p className="settings-card-desc">
                  {assessmentText(
                    "maxScopeInput",
                    "Maximum assessment input bytes",
                    "Assessment input 上限 (bytes)",
                  )}
                  : {assessmentContract.input_scope.max_input_bytes}
                </p>
              </>
            )}
          </section>
          {currentPreview.sourceLabels.length > 0 && (
            <p>
              {t("codingAgent.run.sourceLabels")}: {currentPreview.sourceLabels.join(", ")}
            </p>
          )}
          {currentPreview.repositoryCandidates.length > 0 && (
            <div>
              <p>{t("codingAgent.run.repositoryCandidates")}</p>
              <ul>
                {currentPreview.repositoryCandidates.map((candidate) => {
                  const label = repositoryLabel(candidate);
                  return (
                    <li key={label}>
                      <button
                        type="button"
                        className="issue-link"
                        onClick={() => setManualRepositoryText(label)}
                      >
                        {label}
                      </button>
                    </li>
                  );
                })}
              </ul>
            </div>
          )}
          {currentPreview.blockers.length > 0 && (
            <ul aria-label={t("codingAgent.run.blockers")} className="issues-error">
              {currentPreview.blockers.map((blocker) => (
                <li key={blocker}>
                  {t(`codingAgent.run.blockerExplanations.${blocker}`, {
                    defaultValue: t("codingAgent.run.blockerExplanations.default"),
                  })}{" "}
                  <code>{blocker}</code>
                </li>
              ))}
            </ul>
          )}
          {!canApprove && (
            <p role="status" className="settings-card-desc">
              {!previewMatchesRequest
                ? t("codingAgent.run.planNeedsRefresh")
                : !Array.isArray(issue.acceptanceCriteriaItems)
                  ? assessmentText(
                      "canonicalMissing",
                      "Canonical acceptance criteria are unavailable on this legacy Issue. The joined completion text is not used as a fallback.",
                      "Legacy Issue に canonical acceptance criteria vector がありません。結合済み completion text からは補完しません。",
                    )
                  : !assessmentContract
                    ? assessmentText(
                        "contractUnavailable",
                        "The native approval assessment contract is missing or invalid. Approval is blocked.",
                        "Native approval assessment contract がないか無効です。承認を block します。",
                      )
                    : !canonicalCriteriaMatch
                      ? assessmentText(
                          "canonicalMismatch",
                          "Native criteria do not match the current canonical Issue vector; refresh the Issue before approval.",
                          "Native criteria と現在の canonical Issue vector が一致しません。承認前に Issue を再読込してください。",
                        )
                      : !requestPoliciesMatchContract
                        ? assessmentText(
                            "policyMismatch",
                            "The native preview did not apply every selected criterion policy. Approval is blocked until a matching preview is returned.",
                            "Native preview が選択したすべての criterion policy を反映していません。一致する preview が返るまで承認できません。",
                          )
                        : !providerSelectionMatchesCurrent
                          ? assessmentText(
                              "providerChanged",
                              "The selected model no longer matches the native preview provider; refresh the preview.",
                              "選択中 model が native preview provider と一致しません。preview を再取得してください。",
                            )
                          : !canonicalPlanComplete
                            ? t("codingAgent.run.planDetailsUnavailable")
                            : !backendQuery.data?.enabled
                              ? t("codingAgent.run.backendDisabled")
                              : t("codingAgent.run.notApprovable")}
            </p>
          )}
        </section>
      )}
      {!currentPreview && previewError && !previewing && (
        <button
          type="button"
          className="btn"
          onClick={() => void previewRun()}
          disabled={!profileId}
        >
          {t("codingAgent.run.refreshPlan")}
        </button>
      )}

      {startOutcome && (
        <div role="status" className="settings-card-desc">
          {startOutcome.started
            ? t("codingAgent.run.started", { runId: startOutcome.runId })
            : t("codingAgent.run.startNotConfirmed")}
          {startOutcome.blockers.map((blocker) => (
            <div key={blocker}>
              {t(`codingAgent.run.blockerExplanations.${blocker}`, {
                defaultValue: t("codingAgent.run.blockerExplanations.default"),
              })}{" "}
              <code>{blocker}</code>
            </div>
          ))}
        </div>
      )}
      {actionError && (
        <p role="alert" className="issues-error">
          {actionError}
        </p>
      )}

      <section aria-label={t("codingAgent.run.runs")}>
        <h3>{t("codingAgent.run.runs")}</h3>
        {runsQuery.isPending ? (
          <p role="status">{t("common.loading")}</p>
        ) : runsQuery.isError ? (
          <p role="alert" className="issues-error">
            {t("codingAgent.run.runsUnavailable")} {errorText(runsQuery.error)}
          </p>
        ) : matchingRuns.length === 0 ? (
          <p className="settings-card-desc">{t("codingAgent.run.noRuns")}</p>
        ) : (
          <ul>
            {matchingRuns.map((run) => (
              <li key={run.runId} style={{ marginBottom: 8 }}>
                <code>{run.runId}</code>
                {" · "}
                {t(`codingAgent.run.states.${run.state}`, { defaultValue: run.state })}
                {" · "}
                {t(`codingAgent.run.stages.${run.stage}`, { defaultValue: run.stage })}
                {run.autoDone ? ` · ${t("codingAgent.run.autoDoneApproved")}` : ""}
                {run.state === "awaiting_review" && (
                  <button
                    type="button"
                    className="btn"
                    onClick={() => setSelectedReviewRunId(run.runId)}
                  >
                    {t("codingAgent.review.open")}
                  </button>
                )}
                {run.state === "running" && (
                  <button type="button" className="btn" onClick={() => void cancelRun(run.runId)}>
                    {t("codingAgent.run.cancel")}
                  </button>
                )}
                {run.workspacePath && (
                  <div className="settings-card-desc">
                    {japanese ? "保存先" : "Workspace"}: <code>{run.workspacePath}</code>
                  </div>
                )}
                {run.workspaceBranch && (
                  <div className="settings-card-desc">
                    {japanese ? "ブランチ" : "Branch"}: <code>{run.workspaceBranch}</code>
                  </div>
                )}
                {run.commitId && (
                  <div className="settings-card-desc">
                    Commit: <code>{run.commitId}</code>
                  </div>
                )}
                {run.evidence.map((evidence) => (
                  <div key={evidence.evidenceId} className="settings-card-desc">
                    {evidence.name} · {evidence.sizeBytes} bytes
                    {evidence.saveConfirmed ? ` · ${t("codingAgent.run.evidenceSaved")}` : ""}
                  </div>
                ))}
                {run.evidence.length > 0 && (
                  <button
                    type="button"
                    className="btn"
                    onClick={() => setSelectedEvidenceRunId(run.runId)}
                  >
                    {t("codingAgent.evidence.open")}
                  </button>
                )}
              </li>
            ))}
          </ul>
        )}
      </section>

      {selectedReviewRun && (
        <div>
          <button type="button" className="btn" onClick={() => setSelectedReviewRunId(null)}>
            {t("codingAgent.review.close")}
          </button>
          <CodingAgentReviewPanel key={selectedReviewRun.runId} run={selectedReviewRun} />
        </div>
      )}
      {selectedEvidenceRun && (
        <div>
          <button type="button" className="btn" onClick={() => setSelectedEvidenceRunId(null)}>
            {t("codingAgent.evidence.close")}
          </button>
          <CodingAgentEvidenceViewer key={selectedEvidenceRun.runId} run={selectedEvidenceRun} />
        </div>
      )}

      {confirming && previewSnapshot && (
        <Modal onClose={() => setConfirming(false)} ariaLabel={t("codingAgent.run.approvalDialog")}>
          <div className="modal-head">
            <div className="modal-title">{t("codingAgent.run.approvalDialog")}</div>
          </div>
          <div className="modal-body" style={{ display: "grid", gap: 8 }}>
            <p>{t("codingAgent.run.confirmDigest")}</p>
            <code>{previewSnapshot.preview.digest}</code>
            <p>
              {previewSnapshot.preview.issueTitle} · {previewSnapshot.preview.issueId} · revision{" "}
              {previewSnapshot.preview.issueRevision}
            </p>
            {confirmationAssessmentContract && (
              <section
                aria-label={assessmentText(
                  "title",
                  "Approval assessment contract",
                  "承認時の評価契約",
                )}
              >
                <h4>
                  {assessmentText(
                    "criteria",
                    "Canonical acceptance criteria",
                    "Canonical acceptance criteria",
                  )}
                </h4>
                <ol>
                  {confirmationAssessmentContract.criteria.map((criterion) => (
                    <li key={criterion.criterion_id}>
                      <strong>{criterion.criterion_id}</strong>
                      {" · "}
                      {criterionMethodLabel(criterion.method, t, japanese)}
                      {" · "}
                      {criterion.required
                        ? assessmentText("required", "Required", "必須")
                        : assessmentText("optional", "Optional", "任意")}
                      <p style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>
                        {criterion.statement}
                      </p>
                    </li>
                  ))}
                </ol>
                <h5>
                  {assessmentText(
                    "validationPlan",
                    "Native validation plan",
                    "Native validation plan",
                  )}
                </h5>
                {confirmationAssessmentContract.validation_plan.length === 0 ? (
                  <p>
                    {assessmentText(
                      "noValidations",
                      "No validation definitions were returned. An absent validation is not a passed check.",
                      "Validation definition は返されていません。Validation がないことは check 成功ではありません。",
                    )}
                  </p>
                ) : (
                  <ul>
                    {confirmationAssessmentContract.validation_plan.map((validation) => (
                      <li key={validation.validation_id}>
                        <strong>{validation.validation_id}</strong>
                        {" · "}
                        {validation.required
                          ? assessmentText("required", "Required", "必須")
                          : assessmentText("optional", "Optional", "任意")}
                        {validation.command === null ? (
                          ` · ${assessmentText("validationCommandMissing", "Validation command is unavailable. None is not PASS.", "Validation command は利用できません。None は PASS ではありません。")}`
                        ) : (
                          <>
                            {` · cwd ${validation.command.working_directory} · `}
                            {validation.command.permissions.join(", ")}
                            {" · profile "}
                            <code>{validation.command.profile_digest}</code>
                            {validation.command.argv.map((argument, index) => (
                              // biome-ignore lint/suspicious/noArrayIndexKey: argv has no stable identity and repeated arguments are valid.
                              <code key={`${index}:${argument}`} style={{ display: "block" }}>
                                {argument === "" || /\s/.test(argument)
                                  ? JSON.stringify(argument)
                                  : argument}
                              </code>
                            ))}
                          </>
                        )}
                      </li>
                    ))}
                  </ul>
                )}
                {confirmationAssessmentContract.provider && (
                  <p>
                    {assessmentText(
                      "provider",
                      "Semantic judge provider",
                      "Semantic judge provider",
                    )}
                    : <code>{confirmationAssessmentContract.provider.provider_id}</code> ·{" "}
                    <code>{confirmationAssessmentContract.provider.model_id}</code> ·{" "}
                    <code>{confirmationAssessmentContract.provider.model_version}</code> ·{" "}
                    <code>{confirmationAssessmentContract.provider.config_version}</code>
                  </p>
                )}
                <p>
                  {assessmentText(
                    "scopeDigest",
                    "Approval input-scope digest",
                    "承認時 input-scope digest",
                  )}
                  : <code>{confirmationAssessmentContract.judge_input_scope_digest}</code>
                </p>
                <ul>
                  {confirmationAssessmentContract.input_scope.allowed_fields.map((field) => (
                    <li key={field}>{approvedInputFieldLabel(field, t, japanese)}</li>
                  ))}
                  {confirmationAssessmentContract.input_scope.allowed_sources.map((source) => (
                    <li key={`${source.kind}:${source.identity}`}>
                      {source.kind} · <code>{source.identity}</code> · {source.max_bytes} bytes
                    </li>
                  ))}
                </ul>
                <p className="settings-card-desc">
                  {assessmentText(
                    "scopeDigestMeaning",
                    "This digest binds approval-time allowed inputs only; it is not a digest of any future assessment payload.",
                    "この digest は承認時の許可 input 範囲だけを束縛します。将来の assessment payload digest ではありません。",
                  )}
                </p>
              </section>
            )}
            {previewSnapshot.request.manualRepository && (
              <p>
                {t("codingAgent.run.manualRepositoryConfirmation")}:{" "}
                {repositoryLabel(previewSnapshot.request.manualRepository)}
              </p>
            )}
            <label>
              <input
                type="checkbox"
                checked={userConfirmed}
                onChange={(event) => setUserConfirmed(event.currentTarget.checked)}
              />{" "}
              {t("codingAgent.run.userConfirm")}
            </label>
            {previewSnapshot.request.manualRepository && (
              <label>
                <input
                  type="checkbox"
                  checked={manualRepositoryConfirmed}
                  onChange={(event) => setManualRepositoryConfirmed(event.currentTarget.checked)}
                />{" "}
                {t("codingAgent.run.manualRepositoryConfirm")}
              </label>
            )}
            {previewSnapshot.request.autoDoneRequested && (
              <label>
                <input
                  type="checkbox"
                  checked={autoDoneConfirmed}
                  onChange={(event) => setAutoDoneConfirmed(event.currentTarget.checked)}
                />{" "}
                {t("codingAgent.run.autoDoneConfirm")}
              </label>
            )}
          </div>
          <div className="modal-foot">
            <button
              type="button"
              className="btn"
              disabled={approving}
              onClick={() => setConfirming(false)}
            >
              {t("common.cancel")}
            </button>
            <button
              type="button"
              className="btn primary"
              disabled={
                approving ||
                !canApprove ||
                !confirmationAssessmentContract ||
                !userConfirmed ||
                (Boolean(previewSnapshot.request.manualRepository) && !manualRepositoryConfirmed) ||
                (previewSnapshot.request.autoDoneRequested && !autoDoneConfirmed)
              }
              onClick={() => void approveAndStart()}
            >
              {approving ? t("codingAgent.run.starting") : t("codingAgent.run.confirmStart")}
            </button>
          </div>
        </Modal>
      )}
    </section>
  );
}
