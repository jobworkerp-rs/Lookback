import { describe, expect, it } from "vitest";
import extractWorkers from "../workers/lookback-issue-extract-workers.yaml?raw";
import applyWorkflow from "../workers/workflows/issue-resolution-shared/apply.yaml?raw";
import judgeWorkflow from "../workers/workflows/issue-resolution-shared/judge.yaml?raw";
import originWorkflow from "../workers/workflows/issue-resolution-shared/origin.yaml?raw";
import prepareWorkflow from "../workers/workflows/issue-resolution-shared/prepare.yaml?raw";
import listingWorkflow from "../workers/workflows/issue-resolution-shared/review-candidates.yaml?raw";
import workflow from "../workers/workflows/thread-group-issue-resolution/thread-group-issue-resolution-batch.yaml?raw";

// The group workflow delegates the per-candidate work to shared workers; the
// position of each call stands for every step that worker runs.
const call = (worker: string) => {
  const index = workflow.indexOf(`workerName: ${worker}`);
  expect(index, `no call to ${worker}`).toBeGreaterThan(-1);
  return index;
};
const LISTING = "lookback-issue-resolution-review-candidates";
const PREPARE = "lookback-issue-resolution-prepare";
const ORIGIN = "lookback-issue-resolution-origin";
const JUDGE = "lookback-issue-resolution-judge";
const APPLY = "lookback-issue-resolution-apply";
const everyWorkflow = [
  workflow,
  listingWorkflow,
  prepareWorkflow,
  originWorkflow,
  judgeWorkflow,
  applyWorkflow,
].join("\n");

function blockBetween(source: string, startMarker: string, endMarker: string): string {
  const start = source.indexOf(startMarker);
  expect(start, `missing ${startMarker}`).toBeGreaterThan(-1);
  const end = source.indexOf(endMarker, start);
  expect(end, `missing ${endMarker}`).toBeGreaterThan(start);
  return source.slice(start, end);
}

describe("thread-group issue resolution workflow", () => {
  it("requires a positive explicit group owner for direct workflow runs", () => {
    expect(workflow).toMatch(/required: \[[^\]]*group_owner_user_id/);
    expect(workflow).toMatch(/group_owner_user_id:[\s\S]*?minimum: 1/);
    expect(workflow).toContain("user_id: $workflow.input.group_owner_user_id");
  });

  it("resolves and validates one snapshot value before lineage reads or writes", () => {
    const requiredInputs = workflow.slice(
      workflow.indexOf("required: ["),
      workflow.indexOf("properties:"),
    );
    expect(workflow).toContain("expected_membership_snapshot_digest:");
    expect(workflow).toContain("expected_membership_digest:");
    expect(requiredInputs).not.toContain("expected_membership_snapshot_digest");
    expect(requiredInputs).not.toContain("expected_membership_digest");
    expect(workflow).toContain("expected_snapshot_inputs_valid");
    expect(workflow).toContain("expected_snapshot_aliases_match");
    const resolveSnapshot = workflow.indexOf("resolveExpectedSnapshot:");
    const snapshotValidation = workflow.indexOf("validateExpectedSnapshot:");
    const capabilityCheck = workflow.indexOf("GetThreadGroupCapabilities");
    const initialLineageRead = workflow.indexOf("fetchActiveLineage:");
    const initialMembershipCheck = workflow.indexOf("validateInitialMembership:");
    const candidateRead = call(LISTING);
    const firstMutation = call(APPLY);
    expect(resolveSnapshot).toBeGreaterThan(-1);
    expect(snapshotValidation).toBeGreaterThan(-1);
    expect(resolveSnapshot).toBeLessThan(snapshotValidation);
    expect(snapshotValidation).toBeLessThan(capabilityCheck);
    expect(snapshotValidation).toBeLessThan(initialLineageRead);
    expect(snapshotValidation).toBeLessThan(candidateRead);
    expect(snapshotValidation).toBeLessThan(firstMutation);
    expect(initialMembershipCheck).toBeGreaterThan(initialLineageRead);
    expect(initialMembershipCheck).toBeLessThan(candidateRead);
    expect(initialMembershipCheck).toBeLessThan(firstMutation);
    expect(blockBetween(workflow, "validateInitialMembership:", "collectActiveMembers:")).toContain(
      "$expected_membership_digest",
    );
    expect(blockBetween(workflow, "markGroupChanged:", "applyResolution:")).toContain(
      "$expected_membership_digest",
    );
  });

  it("calls the shared workers in pipeline order on their own channel", () => {
    const order = [LISTING, PREPARE, ORIGIN, JUDGE, APPLY].map(call);
    expect(order).toEqual([...order].sort((a, b) => a - b));
    expect(call(PREPARE)).toBeLessThan(workflow.indexOf("- selectSourceThreads:"));
    expect(call(ORIGIN)).toBeGreaterThan(workflow.indexOf("- restrictMembersToMatchingContext:"));
    expect(call(ORIGIN)).toBeLessThan(workflow.indexOf("- primaryEvidenceSelection:"));
    expect(call(JUDGE)).toBeGreaterThan(workflow.indexOf("- skipWithoutSelectedMember:"));
    expect(call(JUDGE)).toBeLessThan(workflow.indexOf("- recordDryRunDecision:"));
    expect(call(APPLY)).toBeGreaterThan(workflow.indexOf("- markGroupChanged:"));
    for (const worker of [LISTING, PREPARE, ORIGIN, JUDGE, APPLY]) {
      const registration = blockBetween(extractWorkers, `name: ${worker}`, "$file:");
      expect(registration).toContain("channel: issue_resolution_shared");
    }
    // Same keys as before the split, so retries keep deduplicating.
    expect(blockBetween(workflow, "- applyResolution:", "- adoptApplyOutcome:")).toContain(
      '"thread-group-issue-resolution:" + ($workflow.input.group_id | tostring) + ":" + $candidate_id + ":" + ($fresh_membership_digest // "")',
    );
  });

  it("declares the group, endpoint, policy, and LLM inputs and uses generic manager methods", () => {
    expect(extractWorkers).toContain("name: lookback-thread-group-issue-resolution");
    expect(extractWorkers).toContain(
      "$file: workflows/thread-group-issue-resolution/thread-group-issue-resolution-batch.yaml",
    );
    for (const input of [
      "group_id",
      "expected_membership_snapshot_digest",
      "memories_grpc_host",
      "memories_grpc_port",
      "issue_tracker_grpc_host",
      "issue_tracker_grpc_port",
      "policy_json",
      "llm_worker_name",
    ]) {
      expect(workflow).toContain(input);
    }
    expect(workflow).toContain("ThreadGroupService/FindThreadGroupLineage");
    expect(listingWorkflow).toContain("ManagerConsumerService/ListReviewCandidates");
    expect(applyWorkflow).toContain("ManagerConsumerService/ResolveCandidate");
    expect(everyWorkflow).not.toContain("IssueExtract/");
    expect(everyWorkflow).not.toContain("IssueGateway/");
  });

  it("uses only active members and excludes the candidate's source thread from evidence", () => {
    expect(workflow).toContain("THREAD_GROUP_MEMBER_STATE_ACTIVE");
    expect(workflow).toContain("source_thread_ids");
    expect(workflow).toContain("other_member_thread_ids");
    expect(workflow).toContain("source_evidence");

    const evidence = blockBetween(
      workflow,
      "selectOtherMemberThreads:",
      "fetchCrossThreadSummaries:",
    );
    expect(evidence).toContain("$active_member_thread_ids");
    expect(evidence).toContain("$source_thread_ids");
    expect(evidence).toContain("index($member)");
    expect(evidence).toContain("other_member_thread_ids");
    expect(workflow).toContain("skipSameThreadCandidate:");
    expect(workflow).toContain('reason: "no_cross_thread_member"');

    const prompt = blockBetween(
      judgeWorkflow,
      "evaluateCrossThreadResolution:",
      "shapeResolution:",
    );
    expect(prompt).toContain("$raw_input_window");
    expect(prompt).not.toContain("$other_member_summaries");
    expect(prompt).not.toContain("$source_thread_summary");
  });

  it("uses raw cross-thread messages after primary selection and validates evidence ids", () => {
    const raw = judgeWorkflow.indexOf("fetchCrossThreadRawMessages:");
    const final = judgeWorkflow.indexOf("evaluateCrossThreadResolution:");
    const finalPrompt = blockBetween(
      judgeWorkflow,
      "evaluateCrossThreadResolution:",
      "shapeResolution:",
    );
    const schema = blockBetween(finalPrompt, "json_schema: |", "            export:");

    expect(workflow.indexOf("primaryEvidenceSelection:")).toBeLessThan(call(JUDGE));
    expect(raw).toBeGreaterThan(-1);
    expect(final).toBeGreaterThan(raw);
    expect(judgeWorkflow).toContain("FindMemoriesByThreadId");
    expect(judgeWorkflow).toContain('roles: ["ROLE_USER", "ROLE_ASSISTANT"]');
    expect(judgeWorkflow).toContain('content_types: ["TEXT"]');
    expect(judgeWorkflow).toContain("raw_input_window");
    expect(judgeWorkflow).toContain("evidence_memory_ids");
    expect(schema).toContain('"evidence_memory_ids"');
    expect(schema).toContain(
      '"required": ["decision", "rationale", "confidence", "evidence_memory_ids"]',
    );
    expect(judgeWorkflow).toContain("($raw_input_window_ids | index($evidence_id)) != null");
    expect(finalPrompt).toContain("$raw_input_window");
    expect(finalPrompt).not.toContain("other_member_summaries");
  });

  it("requires policy confidence and strictly newer cross-thread evidence", () => {
    expect(workflow).toContain("resolution_min_confidence: $resolution_min_confidence");
    expect(judgeWorkflow).toContain("llm_confidence");
    expect(judgeWorkflow).toContain("confidence_below_threshold");
    expect(originWorkflow).toContain("candidate_source_latest_at");
    expect(workflow).toContain("candidate_source_latest_at: $candidate_source_latest_at");
    expect(judgeWorkflow).toContain("raw_evidence_latest_at");
    expect(judgeWorkflow).toContain("$raw_evidence_latest_at > $candidate_source_latest_at");
    expect(judgeWorkflow).toContain("source_thread_ids");
    expect(judgeWorkflow).toContain("evidence_thread_ids");
    expect(judgeWorkflow).toContain("different_member");
    expect(originWorkflow).toContain("MemoryService/Find");
  });

  it("reads the server-owned label snapshot, never the retired context key", () => {
    // Matching behaviour is exercised in scripts/workers.threadGroupIssueResolutionRuntime.test.ts.
    expect(prepareWorkflow).toContain("lookback.source_label_snapshots_json");
    expect(everyWorkflow).not.toContain("lookback.source_context_snapshots_json");
    expect(judgeWorkflow).toContain("[[issue:");
    expect(workflow).toContain("issue_reference_required");
    expect(judgeWorkflow).toContain("issue_reference_required");
  });

  it("stops a dry run before the first mutation", () => {
    expect(workflow).toMatch(/dry_run: \{ type: boolean, default: false \}/);
    expect(workflow.indexOf("- recordDryRunDecision:")).toBeGreaterThan(-1);
    expect(workflow.indexOf("- recordDryRunDecision:")).toBeLessThan(
      workflow.indexOf("- fetchLineageBeforeApply:"),
    );
  });

  it("skips evidence when either the origin or evidence timestamp is absent", () => {
    const originTiming = blockBetween(
      originWorkflow,
      "shapeCandidateSourceTime:",
      "verifyCandidateOriginCompleteness:",
    );
    const evidenceTiming = blockBetween(
      judgeWorkflow,
      "calculateRawEvidenceTime:",
      "evaluateEvidenceReadiness:",
    );

    expect(originTiming).toContain("candidate_source_time_complete");
    expect(evidenceTiming).toContain("evidence_time_complete");
    expect(originTiming).toContain('reason: "candidate_timestamp_missing"');
    expect(evidenceTiming).toContain('reason: "evidence_timestamp_missing"');
    expect(judgeWorkflow).toContain("created_at: ((.data.createdAt");
    expect(judgeWorkflow).toContain("if . == null then null");
    expect(everyWorkflow).not.toContain("createdAt // .data.created_at // 0 | tonumber");
  });

  it("protects human-touched or invalid review rows before any mutation", () => {
    expect(call(PREPARE)).toBeLessThan(call(APPLY));
    expect(prepareWorkflow).toContain("ManagerConsumerService/ListOperationHistory");
    expect(prepareWorkflow).toContain("ACTOR_KIND_HUMAN");
    expect(prepareWorkflow).toContain("claim");
    expect(prepareWorkflow).toContain("terminal");
    expect(prepareWorkflow).toContain("comment");
    expect(prepareWorkflow).toContain("integrity");
    expect(prepareWorkflow).toContain('reason: "integrity_not_normal"');
    expect(prepareWorkflow).toContain('reason: "not_actionable"');
    expect(prepareWorkflow).toContain('reason: "existing_resolution_pending"');
    expect(prepareWorkflow).toContain('reason: "active_claim"');
    expect(prepareWorkflow.indexOf("- protectActiveClaim:")).toBeLessThan(
      prepareWorkflow.indexOf("- skipNonActionableRow:"),
    );
    expect(workflow).toContain("candidate_outcome_recorded");
  });

  it("checks direct associated-Issue history only for AUTO_REGISTERED candidates", () => {
    const candidateHistory = blockBetween(
      prepareWorkflow,
      "listCandidateOperationHistory:",
      "listAssociatedIssueOperationHistory:",
    );
    const issueHistory = blockBetween(
      prepareWorkflow,
      "listAssociatedIssueOperationHistory:",
      "combineOperationHistory:",
    );
    const combinedHistory = blockBetween(
      prepareWorkflow,
      "combineOperationHistory:",
      "detectProtectedHistory:",
    );

    expect(candidateHistory).toContain("candidate_id: $candidate_id");
    expect(candidateHistory).not.toContain("issue_id: $associated_issue_id");
    expect(issueHistory).toContain("CANDIDATE_STATUS_AUTO_REGISTERED");
    expect(issueHistory).toContain('AUTO_REGISTERED"');
    expect(issueHistory).toContain('$associated_issue_id != ""');
    expect(issueHistory).toContain("issue_id: $associated_issue_id");
    expect(issueHistory).toContain("direct_scope_only: true");
    expect(issueHistory).not.toContain("CANDIDATE_STATUS_PROPOSED");
    expect(combinedHistory).toContain("candidate_history_entries");
    expect(combinedHistory).toContain("issue_history_entries");
    expect(combinedHistory).toContain("history_entries");
  });

  it("records only confirmed mutations and exposes every terminal classification", () => {
    expect(prepareWorkflow).toContain("($candidate.revision // 0)");
    expect(applyWorkflow).toContain("expected_revision: $candidate_revision");
    expect(applyWorkflow).toContain("ManagerConsumerService/GetOperation");
    expect(applyWorkflow).toContain("DURABLE_OPERATION_STATE_COMMITTED");
    expect(workflow).toContain("operation_confirmed");
    expect(workflow).toContain("group_changed_candidates");
    expect(workflow).toContain("conflicted_candidates");
    for (const output of [
      "applied_candidates",
      "protected_candidates",
      "skipped_candidates",
      "conflicted_candidates",
      "group_changed_candidates",
      "failed_candidates",
      "applied:",
      "protected:",
      "skipped:",
      "conflicted:",
      "group_changed:",
      "failed:",
    ]) {
      expect(workflow).toContain(output);
    }
    expect(workflow).toContain("recordApplied");
    expect(workflow).not.toMatch(/recordCompleted:\n\s+if:[\s\S]*resolution_response\.success/);
  });

  it("reads every review candidate page with the same guards as the same-thread workflow", () => {
    const list = blockBetween(
      listingWorkflow,
      "- listReviewCandidates:",
      "- failIfReviewPaginationIncomplete:",
    );
    // Each export key is evaluated against the task output alone, so a key
    // must not refer to a sibling export through `$name`.
    expect(list).not.toContain("$review_response");
    expect(list).toContain("review_page_token");
    expect(list).toContain("page_token: $review_page_token");
    expect(list).toContain("page_size: 100");
    expect(list).toContain("nextPageToken // .success.page.next_page_token");
    expect(list).toContain("review_composite_snapshot");
    expect(list).toContain("review_page_seen_tokens");
    expect(list).not.toContain("totalSize");
    expect(everyWorkflow).not.toContain("page_size: 1000");
    expect(listingWorkflow).toContain("review-pagination-incomplete");
    expect(listingWorkflow).toContain("verifyUniqueReviewCandidates:");
    expect(call(LISTING)).toBeLessThan(workflow.indexOf("resolveEachCandidate:"));
  });

  it("exits before any LLM or manager call when resolution is disabled", () => {
    const disabled = workflow.indexOf("skipIfDisabled:");
    const lineage = workflow.indexOf("fetchActiveLineage:");
    const llm = workflow.indexOf("primaryEvidenceSelection:");
    const manager = call(LISTING);

    expect(disabled).toBeGreaterThan(-1);
    expect(disabled).toBeLessThan(lineage);
    expect(disabled).toBeLessThan(llm);
    expect(disabled).toBeLessThan(manager);
    expect(blockBetween(workflow, "normalizePolicy:", "skipIfDisabled:")).toContain(
      "$policy.extraction_enabled",
    );
    expect(workflow).toMatch(
      /skipIfDisabled:[\s\S]*resolution_enabled[\s\S]*resolution_mode[\s\S]*skipped: disabled[\s\S]*then: exit/,
    );
  });

  it("reloads membership immediately before applying each resolution", () => {
    const list = call(LISTING);
    const llm = call(JUDGE);
    const reload = workflow.indexOf("fetchLineageBeforeApply:");
    const apply = workflow.indexOf("applyResolution:");
    const resolve = call(APPLY);

    expect(list).toBeGreaterThan(-1);
    expect(llm).toBeGreaterThan(list);
    expect(reload).toBeGreaterThan(llm);
    expect(apply).toBeGreaterThan(reload);
    expect(resolve).toBeGreaterThan(apply);
    expect(blockBetween(workflow, "fetchLineageBeforeApply:", "applyResolution:")).toContain(
      "membershipSnapshotDigest",
    );
  });

  it("classifies completion, cancellation, rejection, skip, and failure outcomes", () => {
    expect(workflow).toContain("CANDIDATE_STATUS_PROPOSED");
    expect(judgeWorkflow).toContain("reject:");
    expect(judgeWorkflow).toContain("confirm_associated_issue:");
    expect(judgeWorkflow).toContain('target_state: "ISSUE_STATE_DONE"');
    expect(judgeWorkflow).toContain("cancel_associated_issue:");

    for (const output of [
      "completed_candidate_ids",
      "canceled_candidate_ids",
      "rejected_candidate_ids",
      "skipped_candidate_ids",
      "failed_candidates",
      "completed_count",
      "canceled_count",
      "rejected_count",
      "skipped_count",
      "failed_count",
    ]) {
      expect(workflow).toContain(output);
    }
  });
});
