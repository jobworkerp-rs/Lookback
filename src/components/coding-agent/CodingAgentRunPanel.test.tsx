import { useQueryClient } from "@tanstack/react-query";
import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  codingAgentApproveAndStart,
  codingAgentAttachRun,
  codingAgentCancelRun,
  codingAgentGetBackendStatus,
  codingAgentGetReview,
  codingAgentGetSettings,
  codingAgentListDelegationPlans,
  codingAgentListEvidence,
  codingAgentListRuns,
  codingAgentPreviewRun,
  getLlmSettings,
} from "@/api";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type {
  CodingAgentApprovalAssessmentContract,
  CodingAgentReviewSnapshot,
  CodingAgentRunPreview,
  CodingAgentRunPurpose,
  CodingAgentRunStatus,
  CodingAgentSettings,
  IssueListItem,
} from "@/types/api";
import { CodingAgentRunPanel } from "./CodingAgentRunPanel";

vi.mock("@/api", () => ({
  codingAgentApproveAndStart: vi.fn(),
  codingAgentAttachRun: vi.fn(),
  codingAgentCancelRun: vi.fn(),
  codingAgentGetBackendStatus: vi.fn(),
  codingAgentGetReview: vi.fn(),
  codingAgentGetSettings: vi.fn(),
  codingAgentListEvidence: vi.fn(),
  codingAgentListRuns: vi.fn(),
  codingAgentListDelegationPlans: vi.fn(),
  codingAgentPreviewRun: vi.fn(),
  getLlmSettings: vi.fn(),
}));
const settings: CodingAgentSettings = {
  formatVersion: 1,
  revision: 12,
  activeProfileId: "profile-a",
  semanticJudgeEnabled: true,
  autoDoneEnabled: true,
  profiles: [
    {
      profileId: "profile-a",
      version: 4,
      displayName: "Pinned Linux agent",
      imageRef: "registry.example/agent@sha256:abc",
      allowedImageRefs: ["registry.example/agent@sha256:abc"],
      command: ["agent", "--interactive"],
      containerHome: "/workspace",
      resources: {
        cpuMillis: 2000,
        memoryMib: 4096,
        rootDiskMib: 8192,
        maxParallelRuns: 1,
        useStatic: false,
      },
      mountAllowedRoots: [],
      persistentMounts: [],
      allowedRepositories: [
        { host: "forge.example", organization: "lookback", repository: "agent-app" },
      ],
      credentialReference: "keyring:agent-auth",
    },
  ],
  repositoryHosts: [],
};

const issue: IssueListItem = {
  id: "issue-27",
  revision: "9",
  title: "Preserve the canonical Issue revision",
  description: "Inspect this change before any execution.",
  kind: "TASK",
  priority: "P2",
  state: "OPEN",
  review_state: "unreviewed",
  created_at_ms: 1,
  updated_at_ms: 2,
  completion_criteria: "Run the focused tests",
  acceptanceCriteriaItems: ["Run the focused tests"],
  registration_path: null,
  policy: null,
  proof: null,
  source_labels: ["repo:forge.example/lookback/agent-app"],
  contexts: [{ kind: "REPOSITORY", value: "forge.example/lookback/agent-app", uri: null }],
  claim: null,
};

function assessmentContract(
  purpose: CodingAgentRunPurpose = "investigation",
  criteria: CodingAgentApprovalAssessmentContract["criteria"] = [
    {
      criterion_id: "AC-1",
      statement: "Run the focused tests",
      required: true,
      method: "unknown",
    },
  ],
  validationPlan: CodingAgentApprovalAssessmentContract["validation_plan"] = [],
  provider: CodingAgentApprovalAssessmentContract["provider"] = null,
): CodingAgentApprovalAssessmentContract {
  return {
    schema_version: 1,
    purpose,
    criteria,
    validation_plan: validationPlan,
    provider,
    prompt_policy_version: provider ? "coding-agent-judge-v1" : null,
    input_scope: {
      allowed_fields: ["issue_title", "issue_body"],
      allowed_sources: [
        { kind: "issue_context", identity: `issue:${issue.id}`, max_bytes: 32 * 1024 },
      ],
      max_input_bytes: 128 * 1024,
    },
    judge_input_scope_digest: "b".repeat(64),
  };
}

function preview(overrides: Partial<CodingAgentRunPreview> = {}): CodingAgentRunPreview {
  const purpose = overrides.purpose ?? "investigation";
  return {
    planId: "plan-verified-by-backend",
    digest: "d".repeat(64),
    issueId: issue.id,
    issueRevision: 9,
    issueTitle: issue.title,
    issueState: "open",
    sourceLabels: issue.source_labels,
    settingsRevision: 12,
    profileId: "profile-a",
    profileVersion: 4,
    executionMode: "one_shot",
    purpose,
    publicationPlan: "no_publication",
    selectedRepository: {
      host: "forge.example",
      organization: "lookback",
      repository: "agent-app",
    },
    repositorySelectionReason: null,
    repositoryCandidates: [],
    autoDoneRequested: false,
    assessmentContract: assessmentContract(purpose),
    assessmentContractDigest: "a".repeat(64),
    assessmentProviderSupported: false,
    blockers: [
      "approved_base_commit_unavailable",
      "workflow_execution_identity_unavailable",
      "host_workspace_freeze_unavailable",
    ],
    canApprove: false,
    ...overrides,
  };
}

function runStatus(runId: string, state: CodingAgentRunStatus["state"]): CodingAgentRunStatus {
  return {
    runId,
    issueId: issue.id,
    issueRevision: 9,
    revision: 2,
    executionMode: "one_shot",
    purpose: "investigation",
    state,
    stage:
      state === "running" ? "execution" : state === "awaiting_review" ? "review" : "cancellation",
    profileId: "profile-a",
    profileVersion: 4,
    currentAttemptId: "attempt-1",
    autoDone: false,
    stdoutBytes: 0,
    stderrBytes: 0,
    evidence: [],
  };
}

function reviewSnapshot(reviewRun: CodingAgentRunStatus): CodingAgentReviewSnapshot {
  return {
    run: reviewRun,
    issueState: "awaiting_review",
    issueRevision: reviewRun.issueRevision,
    marker: {
      runId: reviewRun.runId,
      fullCommitId: "a".repeat(40),
      evidenceReferences: ["sealed-reference"],
    },
  };
}

async function clickPreviewButton() {
  const button = screen.getByRole("button", { name: /実行計画をプレビュー|計画を再取得/ });
  await waitFor(() => expect(button).toBeEnabled());
  fireEvent.click(button);
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

function IssuePropHarness() {
  const [currentIssue, setCurrentIssue] = useState(issue);
  return (
    <>
      <button
        type="button"
        onClick={() => setCurrentIssue({ ...issue, title: "Changed Issue title" })}
      >
        Change Issue props
      </button>
      <button type="button" onClick={() => setCurrentIssue(issue)}>
        Restore Issue props
      </button>
      <CodingAgentRunPanel issue={currentIssue} />
    </>
  );
}

describe("CodingAgentRunPanel", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    vi.resetAllMocks();
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      settings,
      profileMountAvailability: [
        { profileId: "profile-a", available: false, blockers: ["mount_root_unavailable"] },
      ],
      availableAgentPresets: [],
    });
    vi.mocked(getLlmSettings).mockResolvedValue({
      mode: "external",
      provider_model: "openai/gpt-4o-2025-01-01",
      api_key_set: true,
      base_url: null,
      max_tokens: null,
      model_max_output_tokens: null,
      temperature: null,
      local_preset_id: null,
      local_model_file: null,
      local_hf_repo: null,
      local_ctx_size: null,
      local_kv_cache_type: null,
      summary_output_guidance: {
        small: {
          daily: { item_chars: 1, total_chars: 1 },
          weekly: { item_chars: 1, total_chars: 1 },
          monthly: { item_chars: 1, total_chars: 1 },
        },
        standard: {
          daily: { item_chars: 1, total_chars: 1 },
          weekly: { item_chars: 1, total_chars: 1 },
          monthly: { item_chars: 1, total_chars: 1 },
        },
        large: {
          daily: { item_chars: 1, total_chars: 1 },
          weekly: { item_chars: 1, total_chars: 1 },
          monthly: { item_chars: 1, total_chars: 1 },
        },
        limits: { daily: 1, weekly: 1, monthly: 1, max_source_memory_ids: 1 },
      },
    });
    vi.mocked(codingAgentGetBackendStatus).mockResolvedValue({
      enabled: false,
      executorVersion: null,
      executorProtocolVersion: null,
      executorFeatures: [],
      appManagedJobworkerpReady: false,
      blockers: ["coding_agent_executor_not_bundled", "host_workspace_freeze_unavailable"],
    });
    vi.mocked(codingAgentListRuns).mockResolvedValue([]);
    vi.mocked(codingAgentListDelegationPlans).mockResolvedValue([]);
    vi.mocked(codingAgentGetReview).mockImplementation(async (runId) =>
      reviewSnapshot(runStatus(runId, "awaiting_review")),
    );
    vi.mocked(codingAgentListEvidence).mockResolvedValue([]);
    vi.mocked(codingAgentAttachRun).mockImplementation(async (runId) => ({
      runId,
      windowLabel: "main",
    }));
    vi.mocked(codingAgentCancelRun).mockImplementation(async (runId) => ({
      run: runStatus(runId, "cancelled"),
    }));
  });

  it("loads a delegated proposal for human preview without starting or approving it", async () => {
    vi.mocked(codingAgentListDelegationPlans).mockResolvedValue([
      {
        delegationPlanId: "delegation-one",
        request: {
          issueId: issue.id,
          profileId: "profile-a",
          purpose: "implementation",
          prompt: "Resolve the requested change",
        },
        issueRevision: 9,
        settingsRevision: 12,
        createdAtMs: 1,
        runId: null,
      },
    ]);
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(preview({ purpose: "implementation" }));
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    fireEvent.click(await screen.findByRole("button", { name: "依頼計画を読み込む" }));
    expect(codingAgentPreviewRun).not.toHaveBeenCalled();
    expect(codingAgentApproveAndStart).not.toHaveBeenCalled();
    await clickPreviewButton();
    await waitFor(() =>
      expect(codingAgentPreviewRun).toHaveBeenCalledWith(
        expect.objectContaining({
          issueId: issue.id,
          profileId: "profile-a",
          purpose: "implementation",
          prompt: "Resolve the requested change",
          executionMode: "one_shot",
        }),
      ),
    );
    expect(codingAgentApproveAndStart).not.toHaveBeenCalled();
  });

  it("does not offer another Issue or an already linked delegation for approval", async () => {
    vi.mocked(codingAgentListDelegationPlans).mockResolvedValue([
      {
        delegationPlanId: "other-issue",
        request: {
          issueId: "other",
          profileId: "profile-a",
          purpose: "implementation",
          prompt: null,
        },
        issueRevision: 1,
        settingsRevision: 12,
        createdAtMs: 1,
        runId: null,
      },
      {
        delegationPlanId: "already-started",
        request: {
          issueId: issue.id,
          profileId: "profile-a",
          purpose: "implementation",
          prompt: null,
        },
        issueRevision: 9,
        settingsRevision: 12,
        createdAtMs: 1,
        runId: "accepted-run",
      },
    ]);
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    await waitFor(() => expect(codingAgentListDelegationPlans).toHaveBeenCalledWith(issue.id));
    expect(screen.queryByRole("button", { name: "依頼計画を読み込む" })).not.toBeInTheDocument();
    expect(codingAgentApproveAndStart).not.toHaveBeenCalled();
  });

  it("reports a delegation-list failure instead of treating it as no proposals", async () => {
    vi.mocked(codingAgentListDelegationPlans).mockRejectedValue(new Error("offline"));
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    expect(
      await screen.findByText("依頼計画を取得できませんでした。再読込してください。"),
    ).toBeInTheDocument();
    expect(codingAgentApproveAndStart).not.toHaveBeenCalled();
  });

  it("links the selected delegation only after explicit human approval", async () => {
    vi.mocked(codingAgentListDelegationPlans).mockResolvedValue([
      {
        delegationPlanId: "delegation-human",
        request: {
          issueId: issue.id,
          profileId: "profile-a",
          purpose: "investigation",
          prompt: "Investigate",
        },
        issueRevision: 9,
        settingsRevision: 12,
        createdAtMs: 1,
        runId: null,
      },
    ]);
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(
      preview({
        baseCommit: "1".repeat(40),
        workspaceBranch: "agent/issue-run",
        imageDigest: `sha256:${"a".repeat(64)}`,
        canApprove: true,
        blockers: [],
      }),
    );
    vi.mocked(codingAgentApproveAndStart).mockResolvedValue({
      started: true,
      runId: "delegated-run",
      blockers: [],
    });
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    fireEvent.click(await screen.findByRole("button", { name: "依頼計画を読み込む" }));
    await clickPreviewButton();
    await waitFor(() => expect(screen.getByRole("button", { name: "承認して開始" })).toBeEnabled());
    fireEvent.click(screen.getByRole("button", { name: "承認して開始" }));
    expect(codingAgentApproveAndStart).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("checkbox", { name: "この実行計画を確認しました" }));
    fireEvent.click(screen.getByRole("button", { name: "確認して開始" }));
    await waitFor(() =>
      expect(codingAgentApproveAndStart).toHaveBeenCalledWith(
        expect.objectContaining({
          delegationPlanId: "delegation-human",
          userConfirmed: true,
        }),
      ),
    );
  });

  it("invalidates the preview when another identical proposal is loaded or its prompt is edited", async () => {
    vi.mocked(codingAgentListDelegationPlans).mockResolvedValue(
      ["first", "second"].map((delegationPlanId) => ({
        delegationPlanId,
        request: {
          issueId: issue.id,
          profileId: "profile-a",
          purpose: "investigation" as const,
          prompt: "Investigate",
        },
        issueRevision: 9,
        settingsRevision: 12,
        createdAtMs: 1,
        runId: null,
      })),
    );
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(
      preview({
        baseCommit: "1".repeat(40),
        workspaceBranch: "agent/issue-run",
        imageDigest: `sha256:${"a".repeat(64)}`,
        canApprove: true,
        blockers: [],
      }),
    );
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    const buttons = await screen.findAllByRole("button", { name: "依頼計画を読み込む" });
    const [first, second] = buttons;
    if (!first || !second) throw new Error("expected two delegation proposals");
    fireEvent.click(first);
    await clickPreviewButton();
    await waitFor(() => expect(screen.getByRole("button", { name: "承認して開始" })).toBeEnabled());
    fireEvent.click(second);
    await waitFor(() =>
      expect(screen.queryByRole("button", { name: "承認して開始" })).not.toBeInTheDocument(),
    );
    await clickPreviewButton();
    await waitFor(() => expect(screen.getByRole("button", { name: "承認して開始" })).toBeEnabled());
    fireEvent.change(screen.getByRole("textbox", { name: "追加 prompt（任意）" }), {
      target: { value: "Changed" },
    });
    await waitFor(() =>
      expect(screen.queryByRole("button", { name: "承認して開始" })).not.toBeInTheDocument(),
    );
    expect(codingAgentApproveAndStart).not.toHaveBeenCalled();
  });

  it("cannot load a delegation whose configured profile no longer exists", async () => {
    vi.mocked(codingAgentListDelegationPlans).mockResolvedValue([
      {
        delegationPlanId: "missing-profile",
        request: {
          issueId: issue.id,
          profileId: "deleted-profile",
          purpose: "investigation",
          prompt: null,
        },
        issueRevision: 9,
        settingsRevision: 12,
        createdAtMs: 1,
        runId: null,
      },
    ]);
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    expect(await screen.findByRole("button", { name: "依頼計画を読み込む" })).toBeDisabled();
    expect(codingAgentPreviewRun).not.toHaveBeenCalled();
    expect(codingAgentApproveAndStart).not.toHaveBeenCalled();
  });

  it("sends explicitly entered validation argv and disables an obsolete preview after invalid edits", async () => {
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(preview());
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    const input = await screen.findByRole("textbox", {
      name: "検証コマンド（1行につきargvのJSON配列）",
    });
    fireEvent.change(input, { target: { value: '["cargo","test","literal $(never)"]' } });
    fireEvent.change(screen.getByRole("spinbutton", { name: "検証タイムアウト（秒）" }), {
      target: { value: "43" },
    });
    const button = screen.getByRole("button", { name: "実行計画をプレビュー" });
    await waitFor(() => expect(button).toBeEnabled());
    fireEvent.click(button);
    await waitFor(() =>
      expect(codingAgentPreviewRun).toHaveBeenCalledWith(
        expect.objectContaining({
          validationDrafts: [
            {
              required: true,
              command: {
                argv: ["cargo", "test", "literal $(never)"],
                workingDirectory: "/workspace",
                timeoutMs: 43000,
              },
            },
          ],
        }),
      ),
    );
    fireEvent.change(input, { target: { value: "cargo test" } });
    expect(
      screen.getByRole("button", { name: /実行計画をプレビュー|計画を再取得/ }),
    ).toBeDisabled();
  });

  it("only enables approval for a preview that preserves the requested validation command", async () => {
    const command = {
      argv: ["python", "-c", "print(1)\nprint(2)", ""],
      working_directory: "/workspace",
      permissions: ["read_workspace" as const, "write_build_output" as const],
      profile_digest: "a".repeat(64),
      timeout_ms: 43000,
    };
    const matching = preview({
      baseCommit: "1".repeat(40),
      workspaceBranch: "agent/issue-run",
      imageDigest: `sha256:${"a".repeat(64)}`,
      canApprove: true,
      blockers: [],
      assessmentContract: assessmentContract("investigation", undefined, [
        { validation_id: "V-1", required: true, command },
      ]),
    });
    vi.mocked(codingAgentPreviewRun)
      .mockResolvedValueOnce({ ...matching, assessmentContract: assessmentContract() })
      .mockResolvedValueOnce(matching);
    vi.mocked(codingAgentApproveAndStart).mockResolvedValue({
      started: true,
      runId: "validation-approved-run",
      blockers: [],
    });
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    fireEvent.change(
      await screen.findByRole("textbox", { name: "検証コマンド（1行につきargvのJSON配列）" }),
      { target: { value: JSON.stringify(command.argv) } },
    );
    fireEvent.change(screen.getByRole("spinbutton", { name: "検証タイムアウト（秒）" }), {
      target: { value: "43" },
    });
    await clickPreviewButton();
    expect(await screen.findByRole("button", { name: "承認して開始" })).toBeDisabled();
    await clickPreviewButton();
    await waitFor(() => expect(screen.getByRole("button", { name: "承認して開始" })).toBeEnabled());
    fireEvent.click(screen.getByRole("button", { name: "承認して開始" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "この実行計画を確認しました" }));
    fireEvent.click(screen.getByRole("button", { name: "確認して開始" }));
    await waitFor(() =>
      expect(codingAgentApproveAndStart).toHaveBeenCalledWith(
        expect.objectContaining({
          preview: expect.objectContaining({
            validationDrafts: [
              {
                required: true,
                command: { argv: command.argv, workingDirectory: "/workspace", timeoutMs: 43000 },
              },
            ],
          }),
          userConfirmed: true,
        }),
      ),
    );
  });

  it("shows the retained local workspace, branch and observed commit", async () => {
    const local = Object.assign(runStatus("run-local", "awaiting_review"), {
      workspacePath: "/tmp/workspaces/issue-27/run-local",
      workspaceBranch: "agent/issue-27-run-local",
      commitId: "a".repeat(40),
    });
    vi.mocked(codingAgentListRuns).mockResolvedValue([local]);
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    expect(await screen.findByText(local.workspacePath)).toBeVisible();
    expect(screen.getByText(local.workspaceBranch)).toBeVisible();
    expect(screen.getByText(local.commitId)).toBeVisible();
  });

  it("keeps the legacy workflow panel one-shot-only when native sessions replace interactive mode", async () => {
    vi.mocked(codingAgentListRuns).mockResolvedValue([
      { ...runStatus("legacy-interactive", "running"), executionMode: "interactive" },
    ]);
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    expect(await screen.findByText(/実行環境は利用できません/)).toBeInTheDocument();
    expect(document.querySelector('option[value="interactive"]')).toBeNull();
    expect(document.querySelector('option[value="one_shot"]')).not.toBeNull();
    expect(await screen.findByText("legacy-interactive")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "端末を開く" })).not.toBeInTheDocument();
  });

  it("renders actual blockers and canonical preview facts without offering start when any gate is missing", async () => {
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(preview());
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);

    expect(await screen.findByText(/実行環境は利用できません/)).toBeInTheDocument();
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "実行計画をプレビュー" })).toBeEnabled(),
    );
    fireEvent.click(screen.getByRole("button", { name: "実行計画をプレビュー" }));
    await waitFor(() => expect(codingAgentPreviewRun).toHaveBeenCalledTimes(1));

    expect(await screen.findByText(issue.id)).toBeInTheDocument();
    expect(screen.getByText("forge.example/lookback/agent-app")).toBeInTheDocument();
    expect(screen.getByText(/承認対象の base commit は未確認/)).toBeInTheDocument();
    expect(
      screen.getByRole("textbox", { name: "検証コマンド（1行につきargvのJSON配列）" }),
    ).toBeInTheDocument();
    expect(screen.getByText(/approved_base_commit_unavailable/)).toBeInTheDocument();
    expect(screen.getByText(/workflow_execution_identity_unavailable/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "承認して開始" })).toBeDisabled();
    expect(screen.queryByText(/Ready/)).not.toBeInTheDocument();
    expect(codingAgentApproveAndStart).not.toHaveBeenCalled();
    expect(codingAgentPreviewRun).toHaveBeenCalledWith({
      issueId: issue.id,
      profileId: "profile-a",
      executionMode: "one_shot",
      purpose: "investigation",
      publicationPlan: "no_publication",
      autoDoneRequested: false,
    });
  });

  it("shows the prepared base, branch and image and enables approval from verified preview facts", async () => {
    const baseCommit = "1".repeat(40);
    const workspaceBranch = "agent/issue-17-run-prepared";
    const imageDigest = `sha256:${"a".repeat(64)}`;
    const prepared = preview({
      baseCommit,
      workspaceBranch,
      imageDigest,
      canApprove: true,
      blockers: [],
    });
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(prepared);
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    await clickPreviewButton();
    expect(await screen.findByText(baseCommit)).toBeInTheDocument();
    expect(screen.getByText(workspaceBranch)).toBeInTheDocument();
    expect(screen.getByText(imageDigest)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "承認して開始" })).toBeEnabled();
  });

  it("renders backend-returned preview identity and plan settings even if the form is edited later", async () => {
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(
      preview({
        profileId: "server-profile",
        profileVersion: 19,
        executionMode: "interactive",
        purpose: "implementation",
        publicationPlan: "create_pull_request",
        autoDoneRequested: false,
        blockers: ["approved_base_commit_unavailable"],
        canApprove: false,
      }),
    );
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "実行計画をプレビュー" })).toBeEnabled(),
    );
    fireEvent.click(screen.getByRole("button", { name: "実行計画をプレビュー" }));
    const plan = await screen.findByRole("region", { name: "backend canonical 実行計画" });

    expect(plan.textContent).toContain("server-profile");
    expect(plan.textContent).toContain("version 19");
    expect(within(plan).getByText("Interactive")).toBeInTheDocument();
    expect(within(plan).getByText("実装")).toBeInTheDocument();
    expect(within(plan).getByText("Pull request を作成")).toBeInTheDocument();
    expect(within(plan).getByText("OFF")).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText("公開計画"), { target: { value: "push_branch" } });
    expect(within(plan).getByText("実装")).toBeInTheDocument();
    expect(within(plan).getByText("Pull request を作成")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "承認して開始" })).toBeNull();
    expect(screen.getByText(/入力が変更されています/)).toBeInTheDocument();
    expect(codingAgentApproveAndStart).not.toHaveBeenCalled();
  });

  it("claims the exact run owner before canceling a recovered one-shot run without subscribing to a terminal feed", async () => {
    vi.mocked(codingAgentListRuns).mockResolvedValue([runStatus("run-one-shot", "running")]);
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    const cancel = await screen.findByRole("button", { name: "取消を要求" });
    fireEvent.click(cancel);

    await waitFor(() => expect(codingAgentCancelRun).toHaveBeenCalledWith("run-one-shot"));
    expect(codingAgentAttachRun).toHaveBeenCalledWith("run-one-shot");
    const attachOrder = vi.mocked(codingAgentAttachRun).mock.invocationCallOrder[0];
    const cancelOrder = vi.mocked(codingAgentCancelRun).mock.invocationCallOrder[0];
    if (attachOrder === undefined || cancelOrder === undefined) {
      throw new Error("run ownership and cancellation calls were not recorded");
    }
    expect(attachOrder).toBeLessThan(cancelOrder);
  });

  it("does not cancel when a recovered run is owned by another window", async () => {
    vi.mocked(codingAgentListRuns).mockResolvedValue([runStatus("run-one-shot", "running")]);
    vi.mocked(codingAgentAttachRun).mockRejectedValueOnce(new Error("owned by another window"));
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    fireEvent.click(await screen.findByRole("button", { name: "取消を要求" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      /別の application window が所有しています/,
    );
    expect(codingAgentAttachRun).toHaveBeenCalledWith("run-one-shot");
    expect(codingAgentCancelRun).not.toHaveBeenCalled();
  });

  it("offers no terminal finish action for a legacy interactive run", async () => {
    vi.mocked(codingAgentListRuns).mockResolvedValue([
      { ...runStatus("run-interactive", "running"), executionMode: "interactive" },
    ]);
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    expect(await screen.findByText("run-interactive")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "端末を開く" })).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "finish-capability-test" }),
    ).not.toBeInTheDocument();
  });

  it("allows investigation auto-DONE under the same semantic setting as implementation", async () => {
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(preview());
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    const autoDone = await screen.findByRole("checkbox", { name: /auto.?DONE|自動.*DONE/i });
    await waitFor(() => expect(autoDone).toBeEnabled());
    fireEvent.click(autoDone);
    await clickPreviewButton();
    expect(codingAgentPreviewRun).toHaveBeenCalledWith(
      expect.objectContaining({
        purpose: "investigation",
        executionMode: "one_shot",
        autoDoneRequested: true,
      }),
    );
  });

  it("disables auto-DONE when semantic judging is disabled", async () => {
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      settings: { ...settings, semanticJudgeEnabled: false, autoDoneEnabled: false },
      profileMountAvailability: [{ profileId: "profile-a", available: true, blockers: [] }],
      availableAgentPresets: [],
    });
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(preview());
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    const autoDone = await screen.findByRole("checkbox", { name: /auto.?DONE|自動.*DONE/i });
    expect(autoDone).toBeDisabled();
    expect(autoDone).not.toBeChecked();
    await clickPreviewButton();
    expect(codingAgentPreviewRun).toHaveBeenCalledWith(
      expect.objectContaining({ autoDoneRequested: false }),
    );
  });

  it("clears the selected auto-DONE when settings turn judging off and does not restore it", async () => {
    function SettingsSwitch() {
      const client = useQueryClient();
      const [enabled, setEnabled] = useState(false);
      return (
        <button
          type="button"
          onClick={() => {
            setEnabled(!enabled);
            client.setQueryData(["coding-agent-settings"], {
              settings: { ...settings, semanticJudgeEnabled: enabled, autoDoneEnabled: enabled },
              profileMountAvailability: [{ profileId: "profile-a", available: true, blockers: [] }],
              availableAgentPresets: [],
            });
          }}
        >
          Toggle judging
        </button>
      );
    }
    renderWithProviders(
      <>
        <CodingAgentRunPanel issue={issue} />
        <SettingsSwitch />
      </>,
    );
    const checkbox = await screen.findByRole("checkbox", { name: /auto.?DONE|自動.*DONE/i });
    await waitFor(() => expect(checkbox).toBeEnabled());
    fireEvent.click(checkbox);
    expect(checkbox).toBeChecked();
    fireEvent.click(screen.getByRole("button", { name: "Toggle judging" }));
    await waitFor(() => expect(checkbox).not.toBeChecked());
    expect(checkbox).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Toggle judging" }));
    await waitFor(() => expect(checkbox).toBeEnabled());
    expect(checkbox).not.toBeChecked();
  });

  it("opens the dedicated review UI only after a human selects a pending run", async () => {
    const reviewRun = runStatus("run-review", "awaiting_review");
    vi.mocked(codingAgentListRuns).mockResolvedValue([reviewRun]);
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    fireEvent.click(await screen.findByRole("button", { name: "Coding agent review を開く" }));

    expect(
      await screen.findByRole("region", { name: "Coding agent の人によるレビュー" }),
    ).toBeInTheDocument();
    expect(codingAgentAttachRun).toHaveBeenCalledWith("run-review");
    expect(codingAgentGetReview).toHaveBeenCalledWith("run-review");
  });

  it("keeps the digest visible but blocks approval when canonical base and validation facts are absent", async () => {
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(
      preview({
        blockers: [],
        canApprove: true,
        purpose: "implementation",
      }),
    );
    vi.mocked(codingAgentApproveAndStart).mockResolvedValue({
      started: false,
      runId: null,
      blockers: ["verified_execution_adapter_unavailable"],
    });
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);

    fireEvent.change(screen.getByLabelText("実行目的"), { target: { value: "implementation" } });
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "実行計画をプレビュー" })).toBeEnabled(),
    );
    fireEvent.click(screen.getByRole("button", { name: "実行計画をプレビュー" }));
    await screen.findByText("plan-verified-by-backend");
    const approve = screen.getByRole("button", { name: "承認して開始" });
    expect(approve).toBeDisabled();
    expect(screen.getByText("d".repeat(64))).toBeInTheDocument();
    expect(
      screen.getByText(/approved base commit と検証計画が含まれていません/),
    ).toBeInTheDocument();
    expect(codingAgentApproveAndStart).not.toHaveBeenCalled();
    expect(screen.queryByRole("region", { name: "Coding agent 端末" })).not.toBeInTheDocument();
    expect(screen.queryByText(/実行を開始しました/)).not.toBeInTheDocument();
  });

  it("keeps the native preview immutable after form edits and fetches a replacement only on explicit click", async () => {
    vi.mocked(codingAgentPreviewRun)
      .mockResolvedValueOnce(preview({ blockers: [], canApprove: true }))
      .mockResolvedValueOnce(preview({ digest: "e".repeat(64), blockers: [], canApprove: true }));
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "実行計画をプレビュー" })).toBeEnabled(),
    );
    fireEvent.click(screen.getByRole("button", { name: "実行計画をプレビュー" }));
    await screen.findByText("plan-verified-by-backend");
    fireEvent.change(screen.getByLabelText("公開計画"), { target: { value: "push_branch" } });
    expect(screen.getByText("plan-verified-by-backend")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "承認して開始" })).toBeNull();
    expect(codingAgentPreviewRun).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "計画を再取得" }));
    await screen.findByText("e".repeat(64));
    expect(codingAgentPreviewRun).toHaveBeenCalledTimes(2);
    expect(codingAgentApproveAndStart).not.toHaveBeenCalled();
  });

  it("shows every native acceptance criterion as one canonical item and keeps approval blocked without runtime proof", async () => {
    const multiline = "Keep the acceptance item intact\nincluding its second line";
    const previewContract = assessmentContract("implementation", [
      {
        criterion_id: "AC-1",
        statement: multiline,
        required: true,
        method: "unknown",
      },
    ]);
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(
      preview({
        purpose: "implementation",
        blockers: [],
        canApprove: true,
        assessmentContract: previewContract,
      }),
    );
    renderWithProviders(
      <CodingAgentRunPanel
        issue={{
          ...issue,
          completion_criteria: multiline,
          acceptanceCriteriaItems: [multiline],
        }}
      />,
    );
    fireEvent.change(screen.getByLabelText("実行目的"), { target: { value: "implementation" } });
    await clickPreviewButton();

    const plan = await screen.findByRole("region", { name: "backend canonical 実行計画" });
    expect(within(plan).getByText("AC-1")).toBeInTheDocument();
    const criterionText = within(plan).getByText(
      (_, element) => element?.tagName === "P" && element.textContent === multiline,
    );
    expect(criterionText).toBeInTheDocument();
    const criterion = criterionText.closest("li");
    if (!criterion) throw new Error("canonical criterion list item is missing");
    expect(within(criterion).getByText("必須")).toBeInTheDocument();
    expect(criterion.textContent).toMatch(/方法:\s*不明/);
    expect(screen.getByRole("button", { name: "承認して開始" })).toBeDisabled();
    expect(
      screen.getByText(/approved base commit と検証計画が含まれていません/),
    ).toBeInTheDocument();
  });

  it("requires a fresh native preview after an explicit method or requiredness change and sends every criterion policy", async () => {
    const issueWithTwoCriteria: IssueListItem = {
      ...issue,
      completion_criteria: "First condition\nSecond condition",
      acceptanceCriteriaItems: ["First condition", "Second condition\ncontinued"],
    };
    const firstCriterion: CodingAgentApprovalAssessmentContract["criteria"][number] = {
      criterion_id: "AC-1",
      statement: "First condition",
      required: true,
      method: "unknown",
    };
    const secondCriterion: CodingAgentApprovalAssessmentContract["criteria"][number] = {
      criterion_id: "AC-2",
      statement: "Second condition\ncontinued",
      required: true,
      method: "unknown",
    };
    const originalCriteria = [firstCriterion, secondCriterion];
    vi.mocked(codingAgentPreviewRun)
      .mockResolvedValueOnce(
        preview({
          assessmentContract: assessmentContract("investigation", originalCriteria),
          blockers: [],
          canApprove: true,
        }),
      )
      .mockResolvedValueOnce(
        preview({
          assessmentContract: assessmentContract("investigation", [
            { ...firstCriterion, required: false, method: "deterministic" },
            secondCriterion,
          ]),
          blockers: [],
          canApprove: true,
        }),
      );
    renderWithProviders(<CodingAgentRunPanel issue={issueWithTwoCriteria} />);
    await clickPreviewButton();
    const plan = await screen.findByRole("region", { name: "backend canonical 実行計画" });

    fireEvent.change(within(plan).getByLabelText("AC-1 評価方法"), {
      target: { value: "deterministic" },
    });
    fireEvent.click(within(plan).getByLabelText("AC-1 必須"));
    expect(screen.getByText(/入力が変更されています/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "承認して開始" })).toBeNull();
    expect(codingAgentApproveAndStart).not.toHaveBeenCalled();

    await clickPreviewButton();
    await waitFor(() => expect(codingAgentPreviewRun).toHaveBeenCalledTimes(2));
    expect(codingAgentPreviewRun).toHaveBeenLastCalledWith(
      expect.objectContaining({
        criterionPolicies: [
          { criterionId: "AC-1", required: false, method: "deterministic" },
          { criterionId: "AC-2", required: true, method: "unknown" },
        ],
      }),
    );
    expect(
      within(plan).getByText(
        (_, element) =>
          element?.tagName === "P" && element.textContent === "Second condition\ncontinued",
      ),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "承認して開始" })).toBeDisabled();
  });

  it("shows native validation limitations, the exact supported provider, and approval-only input scope", async () => {
    const provider = {
      provider_id: "openai",
      model_id: "openai/gpt-4o-2025-01-01",
      model_version: "2025-01-01",
      config_version: "c".repeat(64),
    };
    const supportedContract = assessmentContract(
      "investigation",
      [
        {
          criterion_id: "AC-1",
          statement: "Run the focused tests",
          required: false,
          method: "semantic_judge",
        },
      ],
      [
        {
          validation_id: "V-1",
          required: true,
          command: null,
        },
      ],
      provider,
    );
    supportedContract.input_scope.allowed_fields.push("report", "validation_result");
    supportedContract.input_scope.allowed_sources.push(
      { kind: "report", identity: "agent-report", max_bytes: 1024 * 1024 },
      { kind: "validation_result", identity: "V-1", max_bytes: 32 * 1024 },
    );
    vi.mocked(codingAgentPreviewRun).mockResolvedValueOnce(
      preview({
        assessmentContract: supportedContract,
        assessmentProviderSupported: true,
        blockers: [],
        canApprove: true,
      }),
    );
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    await clickPreviewButton();

    const plan = await screen.findByRole("region", { name: "backend canonical 実行計画" });
    expect(within(plan).getByText("openai")).toBeInTheDocument();
    expect(within(plan).getByText("openai/gpt-4o-2025-01-01")).toBeInTheDocument();
    expect(within(plan).getByText("2025-01-01")).toBeInTheDocument();
    expect(within(plan).getByText("c".repeat(64))).toBeInTheDocument();
    expect(within(plan).getAllByText("V-1")).toHaveLength(2);
    expect(within(plan).getByText(/None は PASS ではありません/)).toBeInTheDocument();
    expect(
      within(plan).getByText(/必須 validation に host result がなければ DONE は block されます/),
    ).toBeInTheDocument();
    expect(within(plan).getByText(/Issue title/)).toBeInTheDocument();
    expect(within(plan).getByText("agent-report")).toBeInTheDocument();
    expect(within(plan).getByRole("heading", { name: "承認時 input scope" })).toBeInTheDocument();
    expect(within(plan).getByText(/将来の assessment payload digest/)).toBeInTheDocument();
    expect(screen.queryByText(/native preview provider と一致しません/)).not.toBeInTheDocument();
  });

  it("keeps an unavailable local provider unapproved without claiming local models are unsupported", async () => {
    vi.mocked(getLlmSettings).mockResolvedValueOnce({
      mode: "local",
      provider_model: null,
      api_key_set: false,
      base_url: null,
      max_tokens: null,
      model_max_output_tokens: null,
      temperature: null,
      local_preset_id: "local-model-alias",
      local_model_file: null,
      local_hf_repo: null,
      local_ctx_size: null,
      local_kv_cache_type: null,
      summary_output_guidance: {
        small: {
          daily: { item_chars: 1, total_chars: 1 },
          weekly: { item_chars: 1, total_chars: 1 },
          monthly: { item_chars: 1, total_chars: 1 },
        },
        standard: {
          daily: { item_chars: 1, total_chars: 1 },
          weekly: { item_chars: 1, total_chars: 1 },
          monthly: { item_chars: 1, total_chars: 1 },
        },
        large: {
          daily: { item_chars: 1, total_chars: 1 },
          weekly: { item_chars: 1, total_chars: 1 },
          monthly: { item_chars: 1, total_chars: 1 },
        },
        limits: { daily: 1, weekly: 1, monthly: 1, max_source_memory_ids: 1 },
      },
    });
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(preview());
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    await clickPreviewButton();
    await screen.findByRole("region", { name: "backend canonical 実行計画" });

    expect(
      screen.getByText(/設定済み worker の準備状態と LLM 設定を確認してください/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(
        (_, element) =>
          element?.tagName === "P" && element.textContent?.includes("local-model-alias"),
      ),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "承認して開始" })).toBeDisabled();
  });

  it("does not revive a native preview after changing profile away and back", async () => {
    const profileA = settings.profiles[0];
    if (!profileA) throw new Error("profile A fixture is missing");
    const profileB = { ...profileA, profileId: "profile-b", displayName: "Second profile" };
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      settings: { ...settings, profiles: [profileA, profileB] },
      profileMountAvailability: [
        { profileId: "profile-a", available: false, blockers: ["mount_root_unavailable"] },
        { profileId: "profile-b", available: false, blockers: ["mount_root_unavailable"] },
      ],
      availableAgentPresets: [],
    });
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(preview());
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    await clickPreviewButton();
    await screen.findByRole("region", { name: "backend canonical 実行計画" });

    const profile = screen.getByLabelText("実行 profile");
    fireEvent.change(profile, { target: { value: "profile-b" } });
    await screen.findByRole("button", { name: "計画を再取得" });
    fireEvent.change(profile, { target: { value: "profile-a" } });

    expect(await screen.findByRole("button", { name: "計画を再取得" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "承認して開始" })).not.toBeInTheDocument();
    expect(
      screen.getByText(/入力が変更されています。承認前に canonical plan を再取得/),
    ).toBeInTheDocument();
    expect(codingAgentPreviewRun).toHaveBeenCalledTimes(1);
  });

  it("does not revive a native preview after an explicit semantic policy round trip", async () => {
    const provider = {
      provider_id: "openai",
      model_id: "openai/gpt-4o-2025-01-01",
      model_version: "2025-01-01",
      config_version: "c".repeat(64),
    };
    const unknownCriterion: CodingAgentApprovalAssessmentContract["criteria"][number] = {
      criterion_id: "AC-1",
      statement: "Run the focused tests",
      required: true,
      method: "unknown",
    };
    const semanticCriterion = { ...unknownCriterion, method: "semantic_judge" as const };
    const unknownContract = assessmentContract("investigation", [unknownCriterion], [], provider);
    const semanticContract = assessmentContract("investigation", [semanticCriterion], [], provider);
    for (const contract of [unknownContract, semanticContract]) {
      contract.input_scope.allowed_fields.push("report");
      contract.input_scope.allowed_sources.push({
        kind: "report",
        identity: "agent-report",
        max_bytes: 1024 * 1024,
      });
    }
    vi.mocked(codingAgentPreviewRun)
      .mockResolvedValueOnce(
        preview({
          assessmentContract: unknownContract,
          assessmentProviderSupported: true,
          blockers: [],
          canApprove: true,
        }),
      )
      .mockResolvedValueOnce(
        preview({
          assessmentContract: semanticContract,
          assessmentProviderSupported: true,
          blockers: [],
          canApprove: true,
        }),
      );
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    await clickPreviewButton();
    const plan = await screen.findByRole("region", { name: "backend canonical 実行計画" });
    fireEvent.change(within(plan).getByLabelText("AC-1 評価方法"), {
      target: { value: "semantic_judge" },
    });
    await clickPreviewButton();
    await waitFor(() => expect(codingAgentPreviewRun).toHaveBeenCalledTimes(2));

    const policy = within(plan).getByLabelText("AC-1 評価方法");
    fireEvent.change(policy, { target: { value: "unknown" } });
    fireEvent.change(policy, { target: { value: "semantic_judge" } });

    expect(await screen.findByRole("button", { name: "計画を再取得" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "承認して開始" })).not.toBeInTheDocument();
    expect(
      screen.getByText(/入力が変更されています。承認前に canonical plan を再取得/),
    ).toBeInTheDocument();
    expect(codingAgentPreviewRun).toHaveBeenCalledTimes(2);
  });

  it("ignores a delayed native preview after its context changes away and back", async () => {
    const pendingPreview = deferred<CodingAgentRunPreview>();
    vi.mocked(codingAgentPreviewRun).mockReturnValueOnce(pendingPreview.promise);
    renderWithProviders(<CodingAgentRunPanel issue={issue} />);
    await clickPreviewButton();
    await waitFor(() => expect(codingAgentPreviewRun).toHaveBeenCalledTimes(1));

    const purpose = screen.getByLabelText("実行目的");
    fireEvent.change(purpose, { target: { value: "implementation" } });
    fireEvent.change(purpose, { target: { value: "investigation" } });
    await act(async () => {
      pendingPreview.resolve(preview({ digest: "f".repeat(64) }));
      await pendingPreview.promise;
    });

    expect(screen.queryByText("f".repeat(64))).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "承認して開始" })).not.toBeInTheDocument();
  });

  it("keeps policy controls disabled and retains required Unknown when the canonical Issue vector is missing", async () => {
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(preview());
    const legacyIssue = { ...issue };
    delete legacyIssue.acceptanceCriteriaItems;
    renderWithProviders(<CodingAgentRunPanel issue={legacyIssue} />);
    await clickPreviewButton();
    const plan = await screen.findByRole("region", { name: "backend canonical 実行計画" });

    expect(
      within(plan).getAllByText(
        /Legacy Issue に canonical acceptance criteria vector がありません/,
      ),
    ).toHaveLength(2);
    expect(within(plan).getByLabelText("AC-1 評価方法")).toBeDisabled();
    expect(within(plan).getByLabelText("AC-1 必須")).toBeDisabled();
    expect(within(plan).getByText(/必須 Unknown criterion は完了を block/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "承認して開始" })).toBeDisabled();
  });

  it("keeps policy controls disabled when the native contract mismatches canonical Issue criteria", async () => {
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(preview());
    renderWithProviders(
      <CodingAgentRunPanel
        issue={{ ...issue, acceptanceCriteriaItems: ["Different canonical item"] }}
      />,
    );
    await clickPreviewButton();
    const plan = await screen.findByRole("region", { name: "backend canonical 実行計画" });

    expect(
      within(plan).getAllByText(/Native criteria と現在の canonical Issue vector が一致しません/),
    ).toHaveLength(2);
    expect(within(plan).getByLabelText("AC-1 評価方法")).toBeDisabled();
    expect(within(plan).getByLabelText("AC-1 必須")).toBeDisabled();
    expect(screen.getByRole("button", { name: "承認して開始" })).toBeDisabled();
  });

  it("invalidates preview context when an approval-relevant Issue prop changes and is restored", async () => {
    vi.mocked(codingAgentPreviewRun).mockResolvedValue(preview());
    renderWithProviders(<IssuePropHarness />);
    await clickPreviewButton();
    await screen.findByRole("region", { name: "backend canonical 実行計画" });

    fireEvent.click(screen.getByRole("button", { name: "Change Issue props" }));
    fireEvent.click(screen.getByRole("button", { name: "Restore Issue props" }));

    expect(await screen.findByRole("button", { name: "計画を再取得" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "承認して開始" })).not.toBeInTheDocument();
    expect(codingAgentPreviewRun).toHaveBeenCalledTimes(1);
  });
});

describe("native local judge approval", () => {
  it.each(["local", "external"] as const)(
    "approves a local provider only in the matching mode (%s)",
    async (mode) => {
      i18n.changeLanguage("ja");
      vi.resetAllMocks();
      vi.mocked(codingAgentGetSettings).mockResolvedValue({
        settings,
        profileMountAvailability: [],
        availableAgentPresets: [],
      });
      vi.mocked(codingAgentListRuns).mockResolvedValue([]);
      vi.mocked(codingAgentListDelegationPlans).mockResolvedValue([]);
      vi.mocked(codingAgentGetBackendStatus).mockResolvedValue({
        enabled: false,
        executorVersion: null,
        executorProtocolVersion: null,
        executorFeatures: [],
        appManagedJobworkerpReady: false,
        blockers: [],
      });
      const current = {
        mode,
        provider_model: null,
        api_key_set: false,
        base_url: null,
        max_tokens: null,
        model_max_output_tokens: null,
        temperature: null,
        local_preset_id: "custom",
        local_model_file: "model.gguf",
        local_hf_repo: "org/model",
        local_ctx_size: null,
        local_kv_cache_type: null,
        summary_output_guidance: {
          small: {
            daily: { item_chars: 1, total_chars: 1 },
            weekly: { item_chars: 1, total_chars: 1 },
            monthly: { item_chars: 1, total_chars: 1 },
          },
          standard: {
            daily: { item_chars: 1, total_chars: 1 },
            weekly: { item_chars: 1, total_chars: 1 },
            monthly: { item_chars: 1, total_chars: 1 },
          },
          large: {
            daily: { item_chars: 1, total_chars: 1 },
            weekly: { item_chars: 1, total_chars: 1 },
            monthly: { item_chars: 1, total_chars: 1 },
          },
          limits: { daily: 1, weekly: 1, monthly: 1, max_source_memory_ids: 1 },
        },
      };
      vi.mocked(getLlmSettings).mockResolvedValue(current);
      vi.mocked(codingAgentPreviewRun).mockResolvedValue(
        preview({
          assessmentContract: assessmentContract("investigation", [], [], {
            provider_id: "local",
            model_id: "resolved-model.gguf",
            model_version: "resolved-model.gguf",
            config_version: "c".repeat(64),
          }),
          assessmentProviderSupported: true,
          blockers: [],
          canApprove: true,
          baseCommit: "1".repeat(40),
          workspaceBranch: "agent/issue-27-run",
          imageDigest: `sha256:${"a".repeat(64)}`,
        }),
      );
      function LocalSettingsHarness() {
        const client = useQueryClient();
        return (
          <>
            <button
              type="button"
              onClick={() =>
                client.setQueryData(["llm-settings"], { ...current, local_ctx_size: 8192 })
              }
            >
              Change context
            </button>
            <button
              type="button"
              onClick={() =>
                client.setQueryData(["llm-settings"], {
                  ...current,
                  local_hf_repo: "another/model",
                })
              }
            >
              Change repository
            </button>
            <button type="button" onClick={() => client.setQueryData(["llm-settings"], current)}>
              Restore settings
            </button>
            <CodingAgentRunPanel issue={{ ...issue, acceptanceCriteriaItems: [] }} />
          </>
        );
      }
      renderWithProviders(<LocalSettingsHarness />);
      await clickPreviewButton();
      await screen.findByRole("region", { name: "backend canonical 実行計画" });
      const approve = screen.getByRole("button", { name: "承認して開始" });
      if (mode === "local") {
        expect(approve).toBeEnabled();
        expect(
          screen.queryByText(
            /現在の Issue に canonical acceptance criteria がないため、承認を block/,
          ),
        ).not.toBeInTheDocument();
      } else expect(approve).toBeDisabled();
      expect(screen.queryByText(/日付付き external model ID のみ/)).not.toBeInTheDocument();
      if (mode === "local") {
        for (const name of ["Change context", "Change repository"]) {
          fireEvent.click(screen.getByRole("button", { name }));
          await screen.findByRole("button", { name: "計画を再取得" });
          fireEvent.click(screen.getByRole("button", { name: "Restore settings" }));
          await waitFor(() =>
            expect(screen.queryByRole("button", { name: "承認して開始" })).not.toBeInTheDocument(),
          );
          await clickPreviewButton();
          expect(await screen.findByRole("button", { name: "承認して開始" })).toBeEnabled();
        }
      }
    },
  );
});
