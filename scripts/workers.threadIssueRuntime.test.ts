import { describe, expect, it } from "vitest";
import workflow from "../workers/workflows/thread-issue-resolution/thread-issue-resolution.yaml?raw";
import {
  expectDuplicateCandidatesRejected,
  expectReviewPagesReleasedBefore,
  mixedPageRows,
  reviewCandidatePageCases,
} from "./lib/reviewCandidatePageFixture";
import { evaluate, runStep, stepCondition } from "./lib/workflowJq";

const message = (id: string, created_at: number) => ({
  id,
  role: "ROLE_USER",
  content_type: "TEXT",
  content: "message",
  created_at,
});

describe("same-thread source snapshot validation", () => {
  it.each([
    ["validateSourceMessageSnapshot", "raw_source_messages"],
    ["validateFreshSourceMessages", "fresh_raw_source_messages"],
  ])("accepts ordered messages and rejects out-of-order messages at %s", (step, field) => {
    const predicate = stepCondition(workflow, step);
    expect(evaluate(predicate, { [field]: [message("1", 100), message("2", 200)] })).toBe(false);
    expect(evaluate(predicate, { [field]: [message("1", 200), message("2", 100)] })).toBe(true);
  });

  it.each([
    ["validateSourceMessageSnapshot", "raw_source_messages"],
    ["validateFreshSourceMessages", "fresh_raw_source_messages"],
  ])("rejects the same malformed source snapshots at %s", (step, field) => {
    const predicate = stepCondition(workflow, step);
    const first = message("1", 100);
    const second = message("2", 200);
    for (const messages of [
      [],
      [first, { ...second, id: "" }],
      [first, { ...second, id: "1" }],
      [first, { ...second, created_at: null }],
      [first, { ...second, role: "ROLE_SYSTEM" }],
      [first, { ...second, content_type: "IMAGE" }],
      [first, { ...second, content: null }],
    ]) {
      expect(evaluate(predicate, { [field]: messages })).toBe(true);
    }
    expect(evaluate(predicate, { [field]: [first, { ...second, created_at: 100 }] })).toBe(false);
  });

  it("requires the stored summary checkpoint to be the current last source message", () => {
    const gate = stepCondition(workflow, "requireSummaryCheckpoint");
    const variables = {
      summary_checkpoint_timestamp: 200,
      summary_checkpoint_matches: true,
      summary_checkpoint_position: 1,
      raw_source_messages: [message("1", 100), message("2", 200)],
    };
    expect(evaluate(gate, variables)).toBe(false);
    expect(evaluate(gate, { ...variables, summary_checkpoint_position: 0 })).toBe(true);
    expect(evaluate(gate, { ...variables, summary_checkpoint_position: null })).toBe(true);
    expect(evaluate(gate, { ...variables, summary_checkpoint_matches: false })).toBe(true);
  });

  it("never cites an oversized or partially presented message as evidence", () => {
    const start = workflow.indexOf("- shapeSameThreadInputWindow:");
    const end = workflow.indexOf("- indexSameThreadInputWindow:", start);
    const body = workflow.slice(start, end);
    const setValueStart = body.indexOf("raw_input_window: >-");
    expect(setValueStart).toBeGreaterThan(-1);
    const expression = body
      .slice(body.indexOf("${", setValueStart) + 2, body.lastIndexOf("}"))
      .trim()
      .replace("(200000) as $limit", "(20) as $limit");
    const oversized = [
      { ...message("1", 100), content: "completed" },
      { ...message("2", 200), content: "completed" + "x".repeat(20) + "not done" },
    ];
    expect(evaluate(expression, { raw_candidate_messages: oversized })).toEqual([]);
  });
});

describe("same-thread review candidate page validation", () => {
  const predicate = stepCondition(workflow, "validateReviewCandidatePage");

  it.each(reviewCandidatePageCases)("$name", ({ bindings, rejected }) => {
    expect(evaluate(predicate, bindings)).toBe(rejected);
  });

  it("does not require a total size anywhere in the review traversal", () => {
    expect(workflow).not.toContain("review_page_total");
    expect(workflow).not.toContain("review_total_size");
    expect(workflow).not.toContain("verifyReviewTotalSize");
    expect(workflow).toContain("verifyUniqueReviewCandidates");
  });
});

describe("same-thread review candidate narrowing", () => {
  // Every jq evaluation copies the whole workflow context, so only this
  // thread's rows may stay in it; all IDs are still kept for the duplicate check.
  const collect = (key: string) =>
    runStep(workflow, "collectReviewCandidatePage", key, {
      workflow: { input: { thread_id: "1" } },
      review_rows: [],
      review_candidate_ids: ["earlier"],
      review_page_rows: mixedPageRows,
      review_page_token: "",
      review_page_seen_tokens: [],
      review_page_next_token: "",
      review_page_count: 0,
      review_composite_snapshot: null,
      review_page_snapshot: {},
    });

  it("keeps only rows citing this thread", () => {
    expect((collect("review_rows") as { candidate: { candidateId: string } }[]).map((r) => r.candidate.candidateId)).toEqual([
      "mine",
      "both",
    ]);
  });

  it("keeps every candidate ID for the uniqueness check", () => {
    expect(collect("review_candidate_ids")).toEqual(["earlier", "mine", "other", "both"]);
    expectDuplicateCandidatesRejected(workflow);
  });

  it("drops page payloads once the traversal is verified", () => {
    expectReviewPagesReleasedBefore(workflow, "resolveSameThreadCandidates");
  });
});

describe("same-thread operation history", () => {
  it("drops raw history pages once the protection flags are derived", () => {
    expect(runStep(workflow, "releaseOperationHistory", "history_page_response", {})).toBeNull();
    for (const key of ["candidate_history_entries", "issue_history_entries", "history_entries"]) {
      expect(runStep(workflow, "releaseOperationHistory", key, {})).toEqual([]);
    }
    const release = workflow.indexOf("- releaseOperationHistory:");
    expect(release).toBeGreaterThan(workflow.indexOf("- detectHumanOperationHistory:"));
    expect(workflow.slice(release).includes("$history_entries")).toBe(false);
  });
});
