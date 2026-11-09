// Shared fixtures for the review-candidate page guards of the same-thread
// and thread-group resolution workflows. `livePage` mirrors a real
// `ManagerConsumerService/ListReviewCandidates` response: the server never
// sets `page.totalSize` for this read model and clamps `page_size` to 100.

import { expect } from "vitest";
import { evaluate, runStep, stepCondition } from "./workflowJq";

function row(id: string): Record<string, unknown> {
  return {
    candidate: {
      candidateId: id,
      status: "CANDIDATE_STATUS_AUTO_REGISTERED",
      title: `title ${id}`,
      description: `description ${id}`,
      sourceEvidence: [{ threadId: "1" }],
    },
    integrity: "REVIEW_CANDIDATE_INTEGRITY_NORMAL",
    resolutionState: "REVIEW_CANDIDATE_RESOLUTION_STATE_ACTIONABLE",
  };
}

const snapshot = {
  backendSnapshot: { snapshotId: "backend-1" },
  managerSnapshot: { snapshotId: "manager-1" },
};

export const livePage = {
  success: {
    rows: [row("a"), row("b")],
    page: { nextPageToken: "token-2", readRevision: "1", snapshot: snapshot.managerSnapshot },
    compositeSnapshot: snapshot,
  },
};

export function pageBindings(
  page: Record<string, unknown>,
  overrides: Record<string, unknown> = {},
): Record<string, unknown> {
  const success = (page.success ?? null) as Record<string, unknown> | null;
  const info = (success?.page ?? null) as Record<string, unknown> | null;
  return {
    review_pages_exhausted: false,
    review_page_rows: success?.rows ?? [],
    review_page_info: info,
    review_page_snapshot: success?.compositeSnapshot ?? null,
    review_page_next_token: String(info?.nextPageToken ?? ""),
    review_page_count: 0,
    review_composite_snapshot: null,
    review_page_seen_tokens: [],
    ...overrides,
  };
}

const withRows = (rows: unknown[]) => ({
  success: { ...livePage.success, rows },
});

export const reviewCandidatePageCases: {
  name: string;
  bindings: Record<string, unknown>;
  rejected: boolean;
}[] = [
  {
    name: "accepts a live page without totalSize",
    bindings: pageBindings(livePage),
    rejected: false,
  },
  {
    name: "accepts a last page with an empty next token",
    bindings: pageBindings({
      success: { ...livePage.success, page: { ...livePage.success.page, nextPageToken: "" } },
    }),
    rejected: false,
  },
  {
    name: "accepts an empty first page",
    bindings: pageBindings({
      success: { rows: [], page: { nextPageToken: "" }, compositeSnapshot: snapshot },
    }),
    rejected: false,
  },
  {
    name: "accepts a later page whose snapshot matches the first page",
    bindings: pageBindings(livePage, { review_page_count: 1, review_composite_snapshot: snapshot }),
    rejected: false,
  },
  {
    name: "rejects a response without success",
    bindings: pageBindings({ error: { code: "UNAVAILABLE" } }),
    rejected: true,
  },
  {
    name: "rejects a page without page info",
    bindings: pageBindings({ success: { rows: livePage.success.rows, compositeSnapshot: snapshot } }),
    rejected: true,
  },
  {
    name: "rejects a page without a composite snapshot",
    bindings: pageBindings({ success: { rows: livePage.success.rows, page: livePage.success.page } }),
    rejected: true,
  },
  {
    name: "rejects a row without a candidate",
    bindings: pageBindings(withRows([row("a"), { integrity: "REVIEW_CANDIDATE_INTEGRITY_NORMAL" }])),
    rejected: true,
  },
  {
    name: "rejects a row with an empty description",
    bindings: pageBindings(
      withRows([{ ...row("a"), candidate: { ...row("a").candidate as object, description: "" } }]),
    ),
    rejected: true,
  },
  {
    name: "rejects a row without integrity",
    bindings: pageBindings(withRows([{ ...row("a"), integrity: "" }])),
    rejected: true,
  },
  {
    name: "rejects a later page whose snapshot drifted from the first page",
    bindings: pageBindings(livePage, {
      review_page_count: 1,
      review_composite_snapshot: {
        ...snapshot,
        managerSnapshot: { snapshotId: "manager-2" },
      },
    }),
    rejected: true,
  },
  {
    name: "rejects a next token that was already visited",
    bindings: pageBindings(livePage, { review_page_seen_tokens: ["token-2"] }),
    rejected: true,
  },
];

// Two pages' worth of rows for the narrowing tests: "mine" cites thread 1,
// "other" cites thread 2, "both" cites 2 and 1.
export const mixedPageRows = [
  { candidate: { candidateId: "mine", sourceEvidence: [{ threadId: "1" }] } },
  { candidate: { candidateId: "other", sourceEvidence: [{ threadId: "2" }] } },
  { candidate: { candidateId: "both", sourceEvidence: [{ threadId: "2" }, { threadId: "1" }] } },
];

// Both workflows must reject a traversal that saw the same candidate twice.
export function expectDuplicateCandidatesRejected(workflow: string): void {
  const unique = stepCondition(workflow, "verifyUniqueReviewCandidates");
  expect(evaluate(unique, { review_candidate_ids: ["a", "a"] })).toBe(true);
  expect(evaluate(unique, { review_candidate_ids: ["a", "b"] })).toBe(false);
}

// Both workflows drop the page payloads once the traversal is verified and
// before their per-candidate loop.
export function expectReviewPagesReleasedBefore(workflow: string, loopStep: string): void {
  expect(runStep(workflow, "releaseReviewPages", "review_page_rows", {})).toEqual([]);
  for (const key of ["review_page_info", "review_page_snapshot"]) {
    expect(runStep(workflow, "releaseReviewPages", key, {})).toBeNull();
  }
  const release = workflow.indexOf("- releaseReviewPages:");
  expect(release).toBeGreaterThan(workflow.indexOf("- verifyUniqueReviewCandidates:"));
  expect(release).toBeLessThan(workflow.indexOf(`- ${loopStep}:`));
}
