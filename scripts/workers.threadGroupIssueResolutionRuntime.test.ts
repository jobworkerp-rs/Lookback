import { describe, expect, it } from "vitest";
import applyWorkflow from "../workers/workflows/issue-resolution-shared/apply.yaml?raw";
import judgeWorkflow from "../workers/workflows/issue-resolution-shared/judge.yaml?raw";
import prepareWorkflow from "../workers/workflows/issue-resolution-shared/prepare.yaml?raw";
import listingWorkflow from "../workers/workflows/issue-resolution-shared/review-candidates.yaml?raw";
import workflow from "../workers/workflows/thread-group-issue-resolution/thread-group-issue-resolution-batch.yaml?raw";
import {
  expectDuplicateCandidatesRejected,
  livePage,
  mixedPageRows,
  reviewCandidatePageCases,
} from "./lib/reviewCandidatePageFixture";
import { blockBetween, evaluate, outputExpression, runStep, stepCondition, stepExpression } from "./lib/workflowJq";

describe("shared review candidate traversal", () => {
  const predicate = stepCondition(listingWorkflow, "validateReviewCandidatePage");

  it.each(reviewCandidatePageCases)("$name", ({ bindings, rejected }) => {
    expect(evaluate(predicate, bindings)).toBe(rejected);
  });

  it("exports page rows and the next token from the gRPC output itself", () => {
    const output = { jsonBody: JSON.stringify(livePage) };
    expect(runStep(listingWorkflow, "readReviewCandidatePage", "review_page_rows", {}, output)).toEqual(
      livePage.success.rows,
    );
    expect(runStep(listingWorkflow, "readReviewCandidatePage", "review_page_next_token", {}, output)).toBe(
      "token-2",
    );
    expect(runStep(listingWorkflow, "readReviewCandidatePage", "review_page_snapshot", {}, output)).toEqual(
      livePage.success.compositeSnapshot,
    );
  });

  it("accumulates only rows citing the requested threads and stops at an empty next token", () => {
    const bindings = {
      source_thread_filter: ["1"],
      review_rows: [{ candidate: { candidateId: "a" } }],
      review_candidate_ids: ["a"],
      review_page_rows: mixedPageRows,
      review_page_token: "token-2",
      review_page_seen_tokens: [],
      review_page_next_token: "",
      review_page_count: 1,
      review_composite_snapshot: null,
      review_page_snapshot: livePage.success.compositeSnapshot,
    };
    const rows = runStep(listingWorkflow, "collectReviewCandidatePage", "review_rows", bindings) as {
      candidate: { candidateId: string };
    }[];
    expect(rows.map((r) => r.candidate.candidateId)).toEqual(["a", "mine", "both"]);
    expect(runStep(listingWorkflow, "collectReviewCandidatePage", "review_candidate_ids", bindings)).toEqual([
      "a",
      "mine",
      "other",
      "both",
    ]);
    expect(runStep(listingWorkflow, "collectReviewCandidatePage", "review_page_seen_tokens", bindings)).toEqual([
      "token-2",
    ]);
    expect(runStep(listingWorkflow, "collectReviewCandidatePage", "review_pages_exhausted", bindings)).toBe(true);
    expect(runStep(listingWorkflow, "collectReviewCandidatePage", "review_composite_snapshot", bindings)).toEqual(
      livePage.success.compositeSnapshot,
    );
  });

  it("keeps every row when no thread filter is given", () => {
    const rows = runStep(listingWorkflow, "collectReviewCandidatePage", "review_rows", {
      source_thread_filter: null,
      review_rows: [],
      review_page_rows: mixedPageRows,
    }) as unknown[];
    expect(rows).toHaveLength(mixedPageRows.length);
  });

  it("fails closed when traversal ends early or returns duplicate candidates", () => {
    expect(evaluate(stepCondition(listingWorkflow, "failIfReviewPaginationIncomplete"), { review_pages_exhausted: false })).toBe(true);
    expect(evaluate(stepCondition(listingWorkflow, "failIfReviewPaginationIncomplete"), { review_pages_exhausted: true })).toBe(false);
    expectDuplicateCandidatesRejected(listingWorkflow);
  });

  it("lets the group ask only for rows citing its active members", () => {
    expect(workflow.indexOf("- listReviewCandidates:")).toBeGreaterThan(workflow.indexOf("active_member_thread_ids: >-"));
    expect(stepExpression(workflow, "listReviewCandidates", "input")).toContain("source_thread_ids: $active_member_thread_ids");
  });
});

const labelSnapshots = (...snapshots: string[][]) => [
  {
    key: "lookback.source_label_snapshots_json",
    value: JSON.stringify(
      snapshots.map((labels, index) => ({ thread_id: `${index + 1}`, extraction_run_id: `run-${index}`, labels })),
    ),
  },
];

describe("thread-group context labels", () => {
  it("reads candidate labels from every source-label snapshot", () => {
    const candidate = { metadata: labelSnapshots(["repo:a/b", "path:/x"], ["path:/y"]) };
    expect(runStep(prepareWorkflow, "selectCandidateFields", "candidate_labels", { candidate })).toEqual([
      "path:/x",
      "path:/y",
      "repo:a/b",
    ]);
  });

  it("treats a missing or corrupt snapshot as unknown labels", () => {
    expect(runStep(prepareWorkflow, "selectCandidateFields", "candidate_labels", { candidate: { metadata: [] } })).toBeNull();
    const corrupt = { metadata: [{ key: "lookback.source_label_snapshots_json", value: "{not json" }] };
    expect(runStep(prepareWorkflow, "selectCandidateFields", "candidate_labels", { candidate: corrupt })).toBeNull();
    // Same rule as the app's source_labels.rs: a snapshot without its identity is invalid.
    const anonymous = {
      metadata: [
        {
          key: "lookback.source_label_snapshots_json",
          value: JSON.stringify([{ thread_id: "", extraction_run_id: "run-1", labels: ["repo:a/b"] }]),
        },
      ],
    };
    expect(runStep(prepareWorkflow, "selectCandidateFields", "candidate_labels", { candidate: anonymous })).toBeNull();
  });

  it("parses candidate and Issue snapshots with the same pipeline", () => {
    const normalized = (expression: string) => expression.replace(/\s+/g, " ");
    const candidate = stepExpression(prepareWorkflow, "selectCandidateFields", "candidate_labels").replace(
      "$candidate.metadata",
      "SOURCE",
    );
    const issue = stepExpression(prepareWorkflow, "shapeIssueLabels", "issue_labels").replace(
      "($associated_issue_record.provenanceMetadata // $associated_issue_record.provenance_metadata)",
      "SOURCE",
    );
    expect(normalized(issue)).toBe(normalized(candidate));
  });

  it("reads issue labels from the copied provenance snapshot", () => {
    const associated_issue_record = { provenanceMetadata: labelSnapshots(["repo:a/b"]) };
    expect(runStep(prepareWorkflow, "shapeIssueLabels", "issue_labels", { associated_issue_record })).toEqual([
      "repo:a/b",
    ]);
  });

  it("reads member labels from ThreadService/Find's nested thread data", () => {
    const response = { data: { id: { value: "7" }, data: { labels: ["repo:a/b"], description: "d" } } };
    expect(
      runStep(workflow, "findMemberThread", "member_labels_for_thread", {}, { jsonBody: JSON.stringify(response) }),
    ).toEqual(["repo:a/b"]);
  });

  it("gives each candidate the labels of its other members only", () => {
    const active_member_labels = [
      { thread_id: "1", labels: ["repo:a/b"] },
      { thread_id: "2", labels: ["repo:a/b"] },
      { thread_id: "3", labels: [] },
    ];
    expect(
      runStep(workflow, "selectMemberLabels", "member_labels", {
        active_member_labels,
        other_member_thread_ids: ["2", "3"],
      }),
    ).toEqual(active_member_labels.slice(1));
  });
});

describe("thread-group project context matching", () => {
  const lookback = "repo:git.example.com/jobworkerp-rs/Lookback-app";
  const context = (
    candidate_labels: string[] | null,
    issue_labels: string[] | null,
    members: Record<string, string[]>,
    candidate_status = "CANDIDATE_STATUS_AUTO_REGISTERED",
  ) =>
    runStep(workflow, "validateProjectContext", "project_context", {
      candidate_status,
      candidate_labels,
      issue_labels,
      member_labels: Object.entries(members).map(([thread_id, labels]) => ({ thread_id, labels })),
    }) as { unknown: boolean; member_match: boolean; matching_member_thread_ids: string[] };

  it.each([
    ["the legacy scp form", "repo:git@git.example.com:jobworkerp-rs/Lookback-app.git"],
    ["an https URL with userinfo, default port and upper-case host", "repo:https://u@Git.example.com:443/jobworkerp-rs/Lookback-app.git"],
    ["the canonical form", lookback],
  ])("matches %s against the canonical repository identity", (_name, memberRepo) => {
    expect(context([lookback], [lookback], { m1: [memberRepo] })).toEqual({
      unknown: false,
      member_match: true,
      matching_member_thread_ids: ["m1"],
    });
  });

  it("keeps only members of the same repository", () => {
    const result = context([lookback], [lookback], {
      m1: ["repo:git@git.example.com:jobworkerp-rs/Lookback-app.git"],
      m2: ["repo:git.example.com/jobworkerp-rs/LLM-Memories"],
    });
    expect(result.matching_member_thread_ids).toEqual(["m1"]);
    expect(context([lookback], [lookback], { m2: ["repo:git.example.com/jobworkerp-rs/LLM-Memories"] })).toMatchObject({
      unknown: false,
      member_match: false,
      matching_member_thread_ids: [],
    });
  });

  it("falls back to exact path equality only when a side has no repo label", () => {
    const candidate = [lookback, "path:/w/agent-app", "dir:agent-app"];
    expect(context(candidate, candidate, { m1: ["path:/w/agent-app"] }).member_match).toBe(true);
    expect(context(candidate, candidate, { m1: ["path:/other/agent-app", "dir:agent-app"] })).toMatchObject({
      unknown: false,
      member_match: false,
    });
    // Both sides carry repo labels, so an equal path cannot override a different repository.
    expect(
      context(candidate, candidate, { m1: ["repo:git.example.com/x/y", "path:/w/agent-app"] }).member_match,
    ).toBe(false);
  });

  it("requires the associated Issue to share the candidate's context", () => {
    expect(context([lookback], ["repo:git.example.com/x/y"], { m1: [lookback] })).toMatchObject({
      unknown: false,
      member_match: false,
      matching_member_thread_ids: [],
    });
  });

  it("ignores Issue labels for a proposed candidate", () => {
    expect(context([lookback], null, { m1: [lookback] }, "CANDIDATE_STATUS_PROPOSED")).toMatchObject({
      unknown: false,
      member_match: true,
    });
  });

  it.each([
    ["the candidate has no snapshot", null, [lookback], { m1: [lookback] }],
    ["the candidate has neither repo nor path labels", ["agent:codex"], [lookback], { m1: [lookback] }],
    ["the associated Issue has no snapshot", [lookback], null, { m1: [lookback] }],
    ["no member has repo or path labels", [lookback], [lookback], { m1: ["agent:codex"] }],
    // A label naming no repository must not make two unrelated threads match.
    ["every repo label is empty", ["repo:"], ["repo:"], { m1: ["repo:"] }],
  ])("is unknown when %s", (_name, candidate, issue, members) => {
    expect(context(candidate, issue, members as Record<string, string[]>).unknown).toBe(true);
  });
});

describe("thread-group resolution writes", () => {
  const audit = (resolution_decision: string) =>
    runStep(judgeWorkflow, "prepareDecisionAudit", "decision_audit", {
      resolution_decision,
      evidence_thread_ids: ["t2", "t3"],
      evidence_memory_ids: ["m9", "m10"],
    }) as { rationale: string; reason: string };

  it("records verified evidence IDs in the audit reason, not the LLM rationale", () => {
    const completed = audit("COMPLETE");
    expect(completed.reason).toContain(completed.rationale);
    expect(completed.reason).toContain("[evidence_thread_ids=t2,t3]");
    expect(completed.reason).toContain("[evidence_memory_ids=m9,m10]");
    expect(completed.rationale).not.toContain("m9");
    expect(audit("CANCEL").rationale).not.toBe(completed.rationale);
  });

  it("reports conversational completion as unverified with fixed text", () => {
    const decision_audit = audit("COMPLETE");
    const payload = runStep(judgeWorkflow, "prepareCompletedResolution", "decision_payload", {
      decision_audit,
      associated_issue: { issueId: "issue-1", revision: 3 },
    }) as { confirm_associated_issue: { completion: Record<string, { reason: string; residual_risk: string }> } };
    expect(Object.keys(payload.confirm_associated_issue.completion)).toEqual(["unverified"]);
    expect(payload.confirm_associated_issue.completion.unverified.reason).toBe(decision_audit.rationale);
    expect(payload.confirm_associated_issue.completion.unverified.residual_risk).not.toBe("");
  });

  it("uses the fixed rationale for rejections and cancellations", () => {
    const decision_audit = audit("CANCEL");
    const bindings = { decision_audit, candidate_id: "c1", associated_issue: { issueId: "issue-1", revision: 3 } };
    const reject = runStep(judgeWorkflow, "prepareProposedReject", "decision_payload", bindings) as {
      reject: { rationale: string };
    };
    const cancel = runStep(judgeWorkflow, "prepareCanceledResolution", "decision_payload", bindings) as {
      cancel_associated_issue: { rationale: string };
    };
    expect(reject.reject.rationale).toBe(decision_audit.rationale);
    expect(cancel.cancel_associated_issue.rationale).toBe(decision_audit.rationale);
  });

  it("sends the audit reason with ResolveCandidate", () => {
    const decision_audit = audit("COMPLETE");
    const applyInput = JSON.parse(
      runStep(workflow, "invokeApply", "input", {
        decision_audit,
        candidate_id: "c1",
        candidate_revision: 4,
        decision_payload: { reject: {} },
        fresh_membership_digest: "digest",
        manager_contract: {},
        manager_store_identity: {},
        workflow: { input: { group_id: "1", issue_tracker_grpc_host: "h", issue_tracker_grpc_port: 1 } },
      }) as string,
    );
    expect(applyInput.audit_reason).toBe(decision_audit.reason);
    expect(applyInput.idempotency_key).toBe("thread-group-issue-resolution:1:c1:digest");
    const body = runStep(applyWorkflow, "invokeResolveCandidate", "json_body", {
      candidate_id: "c1",
      candidate_revision: 4,
      decision_payload: { reject: {} },
      manager_contract: {},
      manager_store_identity: {},
      workflow: { input: applyInput },
    }) as string;
    const context = JSON.parse(body).context;
    expect(context.actor_audit.reason).toBe(decision_audit.reason);
    expect(context.actor_audit.actor_id).toBe("lookback-thread-group-issue-resolution");
    expect(context.idempotency_key).toBe(applyInput.idempotency_key);
  });

  it("keeps the LLM rationale in the workflow output only", () => {
    const applied = runStep(workflow, "recordApplied", "applied_candidates", {
      applied_candidates: [],
      candidate_id: "c1",
      operation_id: "op-1",
      resolution_decision: "COMPLETE",
      resolution_rationale: "the follow-up thread merged the fix",
    });
    expect(applied).toEqual([
      { candidate_id: "c1", operation_id: "op-1", decision: "COMPLETE", rationale: "the follow-up thread merged the fix" },
    ]);
  });
});

describe("thread-group dry run", () => {
  const bindings = {
    candidate_outcome_recorded: false,
    decision_payload: { reject: {} },
    dry_run_candidates: [],
    candidate_id: "c1",
    resolution_decision: "COMPLETE",
    resolution_rationale: "done in a later thread",
    evidence_thread_ids: ["t2"],
    evidence_memory_ids: ["m9"],
  };
  const runs = (dry_run: boolean) =>
    runStep(workflow, "recordDryRunDecision", "if", { ...bindings, workflow: { input: { dry_run } } });

  it("stops before the mutation and records the decision", () => {
    expect(runs(true)).toBe(true);
    expect(runs(false)).toBe(false);
    expect(runStep(workflow, "recordDryRunDecision", "dry_run_candidates", bindings)).toEqual([
      {
        candidate_id: "c1",
        decision: "COMPLETE",
        rationale: "done in a later thread",
        evidence_thread_ids: ["t2"],
        evidence_memory_ids: ["m9"],
      },
    ]);
    expect(runStep(workflow, "recordDryRunDecision", "candidate_outcome_recorded", bindings)).toBe(true);
    expect(outputExpression(workflow, "dry_run_candidates")).toBe("$dry_run_candidates");
  });
});

describe("thread-group unknown context", () => {
  const skips = (associated_issue_id: string) =>
    runStep(workflow, "skipUnknownContextWithoutIssue", "if", {
      candidate_outcome_recorded: false,
      project_context: { unknown: true, member_match: false, matching_member_thread_ids: [] },
      associated_issue_id,
    });

  it("skips a candidate without an Issue before any LLM call", () => {
    expect(skips("")).toBe(true);
    expect(skips("issue-1")).toBe(false);
    expect(workflow.indexOf("- skipUnknownContextWithoutIssue:")).toBeLessThan(
      workflow.indexOf("- primaryEvidenceSelection:"),
    );
  });
});

describe("thread-group primary member selection", () => {
  it("keeps each member summary's thread ID so the selector can answer with it", () => {
    const thread_summaries = [
      { memory: { data: { externalId: "summary:2", content: "fixed the parser" } } },
      { memory: { data: { externalId: "summary:3", content: "" } } },
      { memory: { data: { externalId: "summary:9", content: "other group" } } },
    ];
    expect(
      runStep(workflow, "fetchCrossThreadSummaries", "other_member_summaries", {
        thread_summaries,
        other_member_thread_ids: ["2", "3"],
      }),
    ).toEqual([{ thread_id: "2", summary: "fixed the parser" }]);
  });

  it("shows the selector the candidate's content and each summary's thread ID", () => {
    const prompt = blockBetween(workflow, "- primaryEvidenceSelection:", "json_schema:");
    for (const field of ["candidate.title", "candidate.description", "summary.thread_id", "summary.summary"]) {
      expect(prompt).toContain(field);
    }
  });
});

describe("thread-group post-LLM skips", () => {
  const llm_text = JSON.stringify({
    decision: "COMPLETE",
    rationale: "free text that must not be repeated",
    confidence: 0.95,
    evidence_memory_ids: ["m9"],
  });
  const llm_outcome = { decision: "COMPLETE", confidence: 0.95, evidence_memory_ids: ["m9"] };

  it("keeps the parsed decision without the free-form rationale", () => {
    const outcome = (text: string) =>
      runStep(judgeWorkflow, "shapeResolution", "llm_outcome", {
        llm_json: runStep(judgeWorkflow, "parseResolution", "llm_json", { llm_text: text }),
      });
    expect(outcome(llm_text)).toEqual(llm_outcome);
    expect(outcome("not json")).toBeNull();
    expect(outcome('["not an object"]')).toBeNull();
  });

  it.each([
    ["skipInvalidEvidenceMemoryIds", "evidence_memory_ids_outside_input_window"],
    ["skipNonNewEvidence", "raw_evidence_not_strictly_later"],
    ["skipBelowConfidence", "confidence_below_threshold"],
    ["skipNoResolution", "no_resolution"],
  ])("%s records the LLM outcome with the skip", (step, reason) => {
    expect(
      runStep(judgeWorkflow, step, "skipped_candidates", { skipped_candidates: [], candidate_id: "c1", llm_outcome }),
    ).toEqual([{ candidate_id: "c1", reason, llm: llm_outcome }]);
  });
});

describe("thread-group decision ordering", () => {
  it("skips NO_CHANGE before validating the evidence it was forced to cite", () => {
    const skip = (resolution_decision: string) =>
      runStep(judgeWorkflow, "skipNoResolution", "if", { candidate_outcome_recorded: false, resolution_decision });
    expect(skip("NO_CHANGE")).toBe(true);
    expect(skip("COMPLETE")).toBe(false);
    expect(skip("CANCEL")).toBe(false);
    expect(judgeWorkflow.indexOf("- skipNoResolution:")).toBeLessThan(judgeWorkflow.indexOf("- validateEvidenceMemoryIds:"));
  });
});

describe("thread-group run counts", () => {
  it("counts dry-run decisions as processed", () => {
    const count = evaluate(outputExpression(workflow, "processed_count"), {
      applied_candidates: [],
      dry_run_candidates: [{}, {}],
      protected_candidates: [],
      skipped_candidates: [{}],
      conflicted_candidates: [],
      group_changed_candidates: [],
      failed_candidates: [],
    });
    expect(count).toBe(3);
  });
});
