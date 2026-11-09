import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  codingAgentApplyReviewAction,
  codingAgentAttachRun,
  codingAgentGetReview,
  codingAgentReassess,
} from "@/api";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { CodingAgentReviewSnapshot, CodingAgentRunStatus } from "@/types/api";
import { CodingAgentReviewPanel } from "./CodingAgentReviewPanel";

vi.mock("@/api", () => ({
  codingAgentApplyReviewAction: vi.fn(),
  codingAgentAttachRun: vi.fn(),
  codingAgentGetReview: vi.fn(),
  codingAgentReassess: vi.fn(),
}));

const run: CodingAgentRunStatus = {
  runId: "run-review",
  issueId: "issue-review",
  issueRevision: 21,
  revision: 8,
  executionMode: "one_shot",
  purpose: "implementation",
  state: "awaiting_review",
  stage: "review",
  profileId: "profile-review",
  profileVersion: 3,
  currentAttemptId: "attempt-review",
  autoDone: false,
  stdoutBytes: 10,
  stderrBytes: 0,
  evidence: [],
};

const snapshot: CodingAgentReviewSnapshot = {
  semanticReassessmentAvailable: true,
  run,
  issueState: "awaiting_review",
  issueRevision: 21,
  marker: {
    runId: "run-review",
    fullCommitId: "a".repeat(40),
    evidenceReferences: ["sealed-evidence-1", "sealed-evidence-2"],
  },
};

function renderWithReviewClient() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}>
        <CodingAgentReviewPanel run={run} />
      </QueryClientProvider>
    </I18nextProvider>,
  );
  return queryClient;
}

async function openAndAcknowledgeRequestChanges() {
  const panel = await screen.findByRole("region", { name: "Coding agent human review" });
  fireEvent.click(await within(panel).findByRole("button", { name: "Request changes" }));
  const dialog = await screen.findByRole("dialog", { name: "Confirm coding-agent review" });
  fireEvent.change(within(dialog).getByLabelText("Review rationale"), {
    target: { value: "Please address the reviewed context." },
  });
  const acknowledgement = within(dialog).getByLabelText(
    "I reviewed the current Issue revision and marker",
  );
  fireEvent.click(acknowledgement);
  return { panel, dialog, acknowledgement };
}

describe("CodingAgentReviewPanel", () => {
  beforeEach(() => {
    i18n.changeLanguage("en");
    vi.clearAllMocks();
    vi.mocked(codingAgentAttachRun).mockImplementation(async (runId) => ({
      runId,
      windowLabel: "main",
    }));
    vi.mocked(codingAgentGetReview).mockResolvedValue(snapshot);
    vi.mocked(codingAgentApplyReviewAction).mockResolvedValue({
      outcome: "completed",
      run: { ...run, state: "done", stage: "complete", revision: 9 },
      issueRevision: 22,
    });
  });

  it("requests a paid semantic reassessment only after human confirmation without completing the Issue", async () => {
    vi.mocked(codingAgentReassess).mockImplementation(async (request) => ({
      runId: run.runId,
      attemptId: "attempt-review",
      requestKey: request.requestKey,
      expectedRunRevision: 8,
      issueRevision: 21,
      sourceSha256: "a".repeat(64),
      reportReference: "agent-report",
      assessment: { outcome: "PASS", reason: "Issue resolved" },
    }));
    renderWithProviders(<CodingAgentReviewPanel run={run} />);
    const panel = await screen.findByRole("region", { name: "Coding agent human review" });
    fireEvent.click(await within(panel).findByRole("button", { name: "Reassess with AI" }));
    const dialog = await screen.findByRole("dialog", { name: "Confirm coding-agent review" });
    expect(within(dialog).getByText(/additional LLM cost/i)).toBeInTheDocument();
    expect(codingAgentReassess).not.toHaveBeenCalled();
    expect(within(dialog).queryByLabelText("Review rationale")).not.toBeInTheDocument();
    fireEvent.click(
      within(dialog).getByLabelText("I reviewed the current Issue revision and marker"),
    );
    fireEvent.click(within(dialog).getByRole("button", { name: "Confirm review action" }));
    await waitFor(() => expect(codingAgentReassess).toHaveBeenCalledTimes(1));
    expect(codingAgentReassess).toHaveBeenCalledWith(
      expect.objectContaining({
        runId: "run-review",
        expectedRunRevision: 8,
        expectedIssueRevision: 21,
        expectedMarker: snapshot.marker,
        requestKey: expect.any(String),
      }),
    );
    expect(vi.mocked(codingAgentReassess).mock.calls[0]?.[0]).not.toHaveProperty("rationale");
    expect(codingAgentApplyReviewAction).not.toHaveBeenCalled();
    expect(await screen.findByText("Issue resolved")).toBeInTheDocument();
  });

  it.each([true, false])(
    "explains the reassessment completion behavior before confirmation when autoDone=%s",
    async (autoDone) => {
      vi.mocked(codingAgentGetReview).mockResolvedValue({ ...snapshot, run: { ...run, autoDone } });
      renderWithProviders(<CodingAgentReviewPanel run={{ ...run, autoDone }} />);
      const panel = await screen.findByRole("region", { name: "Coding agent human review" });
      fireEvent.click(await within(panel).findByRole("button", { name: "Reassess with AI" }));
      const dialog = await screen.findByRole("dialog", { name: "Confirm coding-agent review" });
      expect(
        within(dialog).getByText(
          autoDone ? /automatically move this Issue to DONE/i : /remain awaiting review/i,
        ),
      ).toBeInTheDocument();
      expect(codingAgentReassess).not.toHaveBeenCalled();
    },
  );

  it("does not offer semantic reassessment when it is disabled by the backend", async () => {
    vi.mocked(codingAgentGetReview).mockResolvedValue({
      ...snapshot,
      semanticReassessmentAvailable: false,
    });
    renderWithProviders(<CodingAgentReviewPanel run={run} />);
    const panel = await screen.findByRole("region", { name: "Coding agent human review" });
    await within(panel).findByRole("button", { name: "Complete Issue" });
    expect(
      within(panel).queryByRole("button", { name: "Reassess with AI" }),
    ).not.toBeInTheDocument();
    expect(codingAgentReassess).not.toHaveBeenCalled();
  });

  it("keeps a reassessment error visible without automatic replay or Issue mutation", async () => {
    vi.mocked(codingAgentReassess).mockRejectedValue(new Error("provider result unknown"));
    renderWithProviders(<CodingAgentReviewPanel run={run} />);
    const panel = await screen.findByRole("region", { name: "Coding agent human review" });
    fireEvent.click(await within(panel).findByRole("button", { name: "Reassess with AI" }));
    const dialog = await screen.findByRole("dialog", { name: "Confirm coding-agent review" });
    expect(within(dialog).queryByLabelText("Review rationale")).not.toBeInTheDocument();
    fireEvent.click(
      within(dialog).getByLabelText("I reviewed the current Issue revision and marker"),
    );
    fireEvent.click(within(dialog).getByRole("button", { name: "Confirm review action" }));
    expect(await within(panel).findByRole("alert")).toBeInTheDocument();
    expect(codingAgentReassess).toHaveBeenCalledTimes(1);
    expect(codingAgentApplyReviewAction).not.toHaveBeenCalled();
    expect(within(dialog).getByRole("button", { name: "Confirm review action" })).toBeDisabled();
  });

  it("rejects a reassessment response for another request even when the run and revisions match", async () => {
    vi.mocked(codingAgentReassess).mockResolvedValue({
      runId: "run-review",
      attemptId: "attempt-review",
      requestKey: "other-request",
      expectedRunRevision: 8,
      issueRevision: 21,
      sourceSha256: "a".repeat(64),
      reportReference: "report",
      assessment: { outcome: "PASS", reason: "Unrelated judgment" },
    });
    renderWithProviders(<CodingAgentReviewPanel run={run} />);
    const panel = await screen.findByRole("region", { name: "Coding agent human review" });
    fireEvent.click(await within(panel).findByRole("button", { name: "Reassess with AI" }));
    const dialog = await screen.findByRole("dialog", { name: "Confirm coding-agent review" });
    expect(within(dialog).queryByLabelText("Review rationale")).not.toBeInTheDocument();
    fireEvent.click(
      within(dialog).getByLabelText("I reviewed the current Issue revision and marker"),
    );
    fireEvent.click(within(dialog).getByRole("button", { name: "Confirm review action" }));
    expect(await within(panel).findByRole("alert")).toBeInTheDocument();
    expect(within(panel).queryByText("Unrelated judgment")).not.toBeInTheDocument();
  });

  it("loads exact backend review context and only applies a confirmed action against its revisions and marker", async () => {
    renderWithProviders(<CodingAgentReviewPanel run={run} />);
    const panel = await screen.findByRole("region", { name: "Coding agent human review" });
    expect(codingAgentAttachRun).toHaveBeenCalledWith("run-review");
    expect(codingAgentGetReview).toHaveBeenCalledWith("run-review");
    expect(await within(panel).findByText("a".repeat(40))).toBeInTheDocument();
    expect(within(panel).getByText("21")).toBeInTheDocument();
    expect(within(panel).getByText("sealed-evidence-1")).toBeInTheDocument();

    fireEvent.click(within(panel).getByRole("button", { name: "Complete Issue" }));
    const dialog = await screen.findByRole("dialog", { name: "Confirm coding-agent review" });
    fireEvent.change(within(dialog).getByLabelText("Review rationale"), {
      target: { value: "Validated the sealed implementation." },
    });
    fireEvent.change(within(dialog).getByLabelText("Completion evidence summary"), {
      target: { value: "Focused tests passed." },
    });
    expect(within(dialog).getByRole("button", { name: "Confirm review action" })).toBeDisabled();
    fireEvent.click(
      within(dialog).getByLabelText("I reviewed the current Issue revision and marker"),
    );
    fireEvent.click(within(dialog).getByRole("button", { name: "Confirm review action" }));

    await waitFor(() => expect(codingAgentApplyReviewAction).toHaveBeenCalledTimes(1));
    expect(codingAgentApplyReviewAction).toHaveBeenCalledWith({
      runId: "run-review",
      expectedRunRevision: 8,
      expectedIssueRevision: 21,
      expectedMarker: snapshot.marker,
      action: "complete",
      rationale: "Validated the sealed implementation.",
      report: {
        evidence: [
          {
            kind: "test_result",
            summary: "Focused tests passed.",
          },
        ],
      },
    });
    expect(await screen.findByText(/review completed|review を完了/)).toBeInTheDocument();
  });

  it("invalidates confirmation during an actual review refetch and requires confirmation of the new Issue revision", async () => {
    let resolveRefetch: ((next: CodingAgentReviewSnapshot) => void) | undefined;
    vi.mocked(codingAgentGetReview)
      .mockResolvedValueOnce(snapshot)
      .mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            resolveRefetch = resolve;
          }),
      );
    const queryClient = renderWithReviewClient();
    const { panel, dialog, acknowledgement } = await openAndAcknowledgeRequestChanges();
    const confirm = within(dialog).getByRole("button", { name: "Confirm review action" });
    expect(confirm).toBeEnabled();

    let refetch: Promise<void> | undefined;
    act(() => {
      refetch = queryClient.invalidateQueries({ queryKey: ["coding-agent-review", run.runId] });
    });
    await waitFor(() => expect(codingAgentGetReview).toHaveBeenCalledTimes(2));
    expect(confirm).toBeDisabled();

    const refreshed = { ...snapshot, issueRevision: 22 };
    await act(async () => {
      resolveRefetch?.(refreshed);
      await refetch;
    });

    expect(queryClient.getQueryData(["coding-agent-review", run.runId])).toMatchObject(refreshed);
    await waitFor(() => expect(within(panel).getByText("22")).toBeInTheDocument());
    expect(acknowledgement).not.toBeChecked();
    expect(confirm).toBeDisabled();
    expect(codingAgentApplyReviewAction).not.toHaveBeenCalled();

    fireEvent.click(acknowledgement);
    fireEvent.click(confirm);
    await waitFor(() => expect(codingAgentApplyReviewAction).toHaveBeenCalledTimes(1));
    expect(codingAgentApplyReviewAction).toHaveBeenCalledWith(
      expect.objectContaining({ expectedIssueRevision: 22 }),
    );
    expect(within(panel).getByText("22")).toBeInTheDocument();
  });

  it.each([
    ["run revision", { ...snapshot, run: { ...run, revision: 9 } }],
    [
      "sealed marker evidence",
      { ...snapshot, marker: { ...snapshot.marker, evidenceReferences: ["sealed-evidence-3"] } },
    ],
  ] as const)(
    "requires reconfirmation when the %s changes in the query cache",
    async (_label, next) => {
      const queryClient = renderWithReviewClient();
      const { dialog, acknowledgement } = await openAndAcknowledgeRequestChanges();
      const confirm = within(dialog).getByRole("button", { name: "Confirm review action" });
      expect(confirm).toBeEnabled();

      act(() => {
        queryClient.setQueryData(["coding-agent-review", run.runId], next);
      });

      expect(queryClient.getQueryData(["coding-agent-review", run.runId])).toEqual(next);
      const panel = screen.getByRole("region", { name: "Coding agent human review" });
      if (next.run.revision === 9) {
        await waitFor(() => expect(within(panel).getByText("9")).toBeInTheDocument());
      }
      if (next.marker.evidenceReferences[0] === "sealed-evidence-3") {
        await waitFor(() =>
          expect(within(panel).getByText("sealed-evidence-3")).toBeInTheDocument(),
        );
      }
      expect(acknowledgement).not.toBeChecked();
      expect(confirm).toBeDisabled();
      expect(codingAgentApplyReviewAction).not.toHaveBeenCalled();

      fireEvent.click(acknowledgement);
      fireEvent.click(confirm);
      await waitFor(() => expect(codingAgentApplyReviewAction).toHaveBeenCalledTimes(1));
      expect(codingAgentApplyReviewAction).toHaveBeenCalledWith(
        expect.objectContaining({
          expectedRunRevision: next.run.revision,
          expectedMarker: next.marker,
        }),
      );
    },
  );

  it("does not apply an older mutation response to a newer review snapshot", async () => {
    let resolveAction:
      | ((response: {
          outcome: string;
          run: CodingAgentRunStatus | null;
          issueRevision: number;
        }) => void)
      | undefined;
    vi.mocked(codingAgentApplyReviewAction).mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveAction = resolve;
        }),
    );
    const queryClient = renderWithReviewClient();
    const { panel, dialog, acknowledgement } = await openAndAcknowledgeRequestChanges();
    fireEvent.click(within(dialog).getByRole("button", { name: "Confirm review action" }));
    await waitFor(() => expect(codingAgentApplyReviewAction).toHaveBeenCalledTimes(1));
    expect(acknowledgement).not.toBeChecked();

    const refreshed = { ...snapshot, issueRevision: 22 };
    act(() => {
      queryClient.setQueryData(["coding-agent-review", run.runId], refreshed);
    });
    await waitFor(() => expect(within(panel).getByText("22")).toBeInTheDocument());
    await act(async () => {
      resolveAction?.({
        outcome: "completed",
        run: { ...run, state: "done", stage: "complete", revision: 9 },
        issueRevision: 22,
      });
    });

    expect(
      await within(panel).findByRole("button", { name: "Reload review context" }),
    ).toBeEnabled();
    expect(within(panel).queryByText(/review completed/i)).not.toBeInTheDocument();
    expect(codingAgentApplyReviewAction).toHaveBeenCalledWith(
      expect.objectContaining({ expectedIssueRevision: 21 }),
    );
  });

  it("keeps a stale review blocked until explicit reload and does not retry the previous action", async () => {
    vi.mocked(codingAgentApplyReviewAction).mockRejectedValueOnce(
      new Error("Issue or review marker changed; refresh before acting"),
    );
    vi.mocked(codingAgentGetReview)
      .mockResolvedValueOnce(snapshot)
      .mockResolvedValueOnce({
        ...snapshot,
        issueRevision: 22,
        issueState: "awaiting_review",
      });
    renderWithProviders(<CodingAgentReviewPanel run={run} />);
    const panel = await screen.findByRole("region", { name: "Coding agent human review" });
    fireEvent.click(await within(panel).findByRole("button", { name: "Request changes" }));
    const dialog = await screen.findByRole("dialog", { name: "Confirm coding-agent review" });
    fireEvent.change(within(dialog).getByLabelText("Review rationale"), {
      target: { value: "Please add the missing edge case." },
    });
    fireEvent.click(
      within(dialog).getByLabelText("I reviewed the current Issue revision and marker"),
    );
    fireEvent.click(within(dialog).getByRole("button", { name: "Confirm review action" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(/changed|更新/);
    expect(within(panel).getByRole("button", { name: "Reload review context" })).toBeEnabled();
    expect(codingAgentApplyReviewAction).toHaveBeenCalledTimes(1);
    expect(codingAgentGetReview).toHaveBeenCalledTimes(1);

    fireEvent.click(within(panel).getByRole("button", { name: "Reload review context" }));
    await waitFor(() => expect(codingAgentGetReview).toHaveBeenCalledTimes(2));
    expect(codingAgentApplyReviewAction).toHaveBeenCalledTimes(1);
    expect(within(panel).getByText("22")).toBeInTheDocument();
  });

  it("surfaces an existing owner conflict without reading review context or applying an action", async () => {
    vi.mocked(codingAgentAttachRun).mockRejectedValueOnce(
      new Error("coding-agent run is owned by another application window"),
    );
    renderWithProviders(<CodingAgentReviewPanel run={run} />);

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "This run is owned by another application window. Review context was not read.",
    );
    expect(codingAgentGetReview).not.toHaveBeenCalled();
    expect(codingAgentApplyReviewAction).not.toHaveBeenCalled();
  });
});
