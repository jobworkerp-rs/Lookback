import { describe, expect, it } from "vitest";
import extractWorkers from "../workers/lookback-issue-extract-workers.yaml?raw";
import workflow from "../workers/workflows/thread-issue-resolution/thread-issue-resolution.yaml?raw";

function blockBetween(source: string, startMarker: string, endMarker: string): string {
  const start = source.indexOf(startMarker);
  expect(start, `missing ${startMarker}`).toBeGreaterThan(-1);
  const end = source.indexOf(endMarker, start);
  expect(end, `missing ${endMarker}`).toBeGreaterThan(start);
  return source.slice(start, end);
}

function indentationOf(source: string, marker: string, startAt = 0): number {
  const markerIndex = source.indexOf(marker, startAt);
  expect(markerIndex, `missing ${marker}`).toBeGreaterThan(-1);
  const lineStart = source.lastIndexOf("\n", markerIndex) + 1;
  const indentation = source.slice(lineStart, markerIndex);
  expect(indentation.trim(), `${marker} must start a YAML line`).toBe("");
  return indentation.length;
}

function resolutionResponseSchema(source: string): Record<string, unknown> {
  const schemaBlock = blockBetween(source, "json_schema: |", "                  export:");
  const schema = schemaBlock
    .split("\n")
    .slice(1)
    .filter((line) => line.trim() !== "")
    .map((line) => line.trimStart())
    .join("\n");

  return JSON.parse(schema) as Record<string, unknown>;
}

describe("same-thread issue resolution workflow", () => {
  it("does not report an early disabled or unavailable exit as committed", () => {
    expect(workflow).toContain("result: '$" + "{ $result // null }'");
    expect(workflow).toContain("assessment_ready: '$" + "{ false }'");
    expect(workflow).toContain("assessment_ready: '$" + "{ true }'");
    for (const step of [
      "skipIfDisabled:",
      "failOnInvalidResolutionPolicy:",
      "skipIfTrackerUnavailable:",
    ]) {
      const start = workflow.indexOf(step);
      const end = workflow.indexOf("then: exit", start);
      expect(start).toBeGreaterThan(-1);
      expect(end).toBeGreaterThan(start);
      expect(workflow.slice(start, end)).toContain("set:");
    }
    expect(workflow).toContain("complete: '${ $assessment_ready");
  });
  it("does not mutate hidden experimental Issues in a normal release build", () => {
    const gate = blockBetween(workflow, "skipIfDisabled:", "failOnInvalidResolutionPolicy:");
    expect(gate).toContain('env.LOOKBACK_EXPERIMENTAL_ISSUES_ENABLED != "true"');
  });
  it("keeps revision conflicts out of a confirmed run result", () => {
    const output = workflow.slice(workflow.lastIndexOf("\noutput:"));
    expect(output.match(/complete: ([^\n]*)/)?.[1]).toContain("$conflicted_candidates | length");
  });
  it("reports an already pending tracker operation as unconfirmed rather than complete", () => {
    const pending = blockBetween(workflow, "protectPendingResolution:", "skipNonActionableState:");
    expect(pending).toContain("REVIEW_CANDIDATE_RESOLUTION_STATE_RESOLUTION_PENDING");
    expect(pending).toContain("unconfirmed_candidates:");
    expect(pending).toContain("candidate_outcome_recorded:");
  });
  it("fails the run rather than silently dropping a candidate after an unexpected error", () => {
    const candidateLoop = blockBetween(workflow, "resolveSameThreadCandidates:", "initCandidate:");
    expect(candidateLoop).not.toContain("onError: continue");
  });
  it("selects the Manager store enum and classifies invalid confidence before threshold failures", () => {
    const storeSelection = blockBetween(
      workflow,
      "selectManagerStore:",
      "validateManagerContract:",
    );
    expect(storeSelection).toContain("(.role // 0) == 2");
    expect(workflow.indexOf("skipInvalidResolutionConfidence:")).toBeLessThan(
      workflow.indexOf("skipBelowResolutionConfidence:"),
    );
  });
  it("registers a dedicated worker with source and saved-summary snapshot inputs", () => {
    expect(extractWorkers).toContain("name: lookback-thread-issue-resolution");
    expect(extractWorkers).toContain(
      "$file: workflows/thread-issue-resolution/thread-issue-resolution.yaml",
    );
    for (const input of [
      "thread_id",
      "summary_memory_id",
      "summary_content_digest",
      "summary_checkpoint_memory_id",
      "summary_checkpoint_at",
      "memories_grpc_host",
      "issue_tracker_grpc_host",
      "policy_json",
      "llm_worker_name",
    ]) {
      expect(workflow).toContain(input);
    }
    expect(workflow).toContain("ManagerConsumerService/ListReviewCandidates");
    expect(workflow).toContain("ManagerConsumerService/ResolveCandidate");
    expect(workflow).toContain("ManagerConsumerService/GetOperation");
    expect(workflow).not.toContain("prepared_decisions");
    expect(workflow).toContain("applyPreparedDecision:");
    expect(workflow).toContain('application_mode: "APPLY_AFTER_COMMIT"');
    expect(workflow).not.toContain("ThreadGroupService/");
    expect(workflow).not.toContain("other_member_thread_ids");
  });

  it("gates only on resolution policy and never on extraction eligibility", () => {
    const policy = blockBetween(workflow, "normalizePolicy:", "skipIfDisabled:");
    const disabled = workflow.indexOf("skipIfDisabled:");
    const review = workflow.indexOf("ManagerConsumerService/ListReviewCandidates");
    const llm = workflow.indexOf("evaluateSameThreadResolution:");
    const resolve = workflow.indexOf("ManagerConsumerService/ResolveCandidate");

    expect(policy).toContain("resolution_mode");
    expect(policy).toContain("resolution_min_confidence");
    expect(workflow).not.toContain("extraction_enabled");
    expect(disabled).toBeGreaterThan(-1);
    expect(disabled).toBeLessThan(review);
    expect(disabled).toBeLessThan(llm);
    expect(disabled).toBeLessThan(resolve);
    expect(workflow).toMatch(/resolution_mode != "APPLY"[\s\S]*then: exit/);
  });

  it("reads every review and history page, or fails closed when traversal is incomplete", () => {
    expect(workflow).toContain("review_page_token");
    expect(workflow).toContain("nextPageToken // .success.page.next_page_token");
    expect(workflow).toContain("page_token: $review_page_token");
    expect(workflow).toContain("review_pages_exhausted");
    expect(workflow).toContain("review-pagination-incomplete");
    expect(workflow).toContain("history_page_token");
    expect(workflow).toContain("direct_scope_only: true");
    expect(workflow).toContain("history-pagination-incomplete");
    expect(workflow).toContain("ListOperationHistory");
    expect(workflow).toContain("review_candidate_ids | length");
    expect(workflow).toContain("history_total_size | tonumber");
    expect(workflow.match(/in: '\$\{ \[range\(0; 1000\)\] \}'/g)).toHaveLength(2);
    const reviewPage = blockBetween(
      workflow,
      "validateReviewCandidatePage:",
      "collectReviewCandidatePage:",
    );
    // ListReviewCandidates never reports totalSize, so completeness comes
    // from token exhaustion, snapshot agreement, and unique candidate IDs.
    expect(reviewPage).not.toContain("review_page_total");
    expect(reviewPage).toContain("review_composite_snapshot");
    expect(reviewPage).toContain("review_page_seen_tokens");
    expect(workflow).toContain("page_size: 100");
    expect(workflow).not.toContain("page_size: 1000");
  });

  it("keeps nested read-failure tasks and gRPC arguments at their YAML parent level", () => {
    const readFailureMarker = workflow.indexOf("- markCurrentIssueReadIncomplete:");
    const failureStepIndent = indentationOf(workflow, "- markCurrentIssueReadIncomplete:");
    const failureSetIndent = indentationOf(workflow, "set:", readFailureMarker);
    const failureFieldIndent = indentationOf(
      workflow,
      "current_issue_read_complete:",
      readFailureMarker,
    );
    const recheckStart = workflow.indexOf("- recheckIssueClaimAndMapping:");
    const recheckBlock = blockBetween(
      workflow,
      "- recheckIssueClaimAndMapping:",
      "- verifyNoClaimOrMappingDrift:",
    );
    const argumentsIndent = indentationOf(recheckBlock, "arguments:");
    const asJsonIndent = indentationOf(recheckBlock, "as_json:");

    expect(failureSetIndent).toBe(failureStepIndent + 4);
    expect(failureFieldIndent).toBe(failureSetIndent + 2);
    expect(recheckStart).toBeGreaterThan(-1);
    expect(asJsonIndent).toBe(argumentsIndent + 2);
  });

  it("does not require timestamps to be unique for a valid checkpoint locator", () => {
    const checkpoint = blockBetween(
      workflow,
      "validateSummaryCheckpoint:",
      "requireSummaryCheckpoint:",
    );

    expect(checkpoint).toContain("summary_checkpoint_memory_id");
    expect(checkpoint).not.toContain("map(select(.created_at == (try");
    expect(workflow).toContain("summary_checkpoint_position");
    expect(workflow).toContain(".source_position <= $summary_checkpoint_position");
  });

  it("only accepts actionable candidates whose complete origin set belongs to this thread", () => {
    expect(workflow).toContain("REVIEW_CANDIDATE_INTEGRITY_NORMAL");
    expect(workflow).toContain("REVIEW_CANDIDATE_RESOLUTION_STATE_ACTIONABLE");
    expect(workflow).toContain("all($source_evidence_refs[];");
    expect(workflow).toContain("source_origins_same_thread");
    expect(workflow).toContain("$workflow.input.thread_id | tostring");
    expect(workflow).toContain("source_origin_complete");
    expect(workflow).toContain("origin_fetch_errors");
    expect(workflow).toContain("CANDIDATE_STATUS_PROPOSED");
    expect(workflow).toContain("CANDIDATE_STATUS_AUTO_REGISTERED");
  });

  it("uses only same-thread raw text messages and validates evidence after every origin locator", () => {
    expect(workflow).toContain("FindMemoriesByThreadId");
    expect(workflow).toContain('roles: ["ROLE_USER", "ROLE_ASSISTANT"]');
    expect(workflow).toContain('content_types: ["TEXT"]');
    expect(workflow).toContain("summary_checkpoint_memory_id");
    expect(workflow).toContain("summary_checkpoint_at");
    expect(workflow).toContain("candidate_source_latest_at");
    expect(workflow).toContain("all($evidence_messages[];");
    expect(workflow).toContain(".created_at > $candidate_source_latest_at");
    expect(workflow).toContain("raw_input_window_ids");
    expect(workflow).toContain("evidence_memory_ids_outside_input_window");
    expect(workflow).toContain("evidence_thread_ids");
    expect(workflow).toContain("source_origins_same_thread");
    expect(workflow).toContain("created_at > $candidate_source_latest_at");
    expect(workflow).toContain("created_at <= $summary_checkpoint_timestamp");
    expect(workflow).toContain("summary_checkpoint_matches");
    expect(workflow).toContain("$candidate.acceptanceCriteria // $candidate.acceptance_criteria");
    expect(workflow).toContain("confidence_out_of_range");
  });

  it("protects human-touched candidates, active claims, invalid mappings, and ineligible issues", () => {
    const historyProtection = blockBetween(
      workflow,
      "detectHumanOperationHistory:",
      "failOnIncompleteHistory:",
    );

    expect(workflow).toContain("ACTOR_KIND_HUMAN");
    expect(workflow).toContain("human_operation_history");
    expect(historyProtection).toContain("ACTOR_KIND_EXTERNAL_AGENT");
    expect(historyProtection).toContain("EXTERNAL_AGENT");
    expect(historyProtection).toContain("DURABLE_OPERATION_STATE_ARBITRATED");
    expect(workflow).toContain("arbitrated_operation_history");
    expect(workflow).toContain("GetIssueWithSync");
    expect(workflow).toContain("$current_issue.claim // null");
    expect(workflow).toContain("active_claim");
    expect(workflow).toContain("current_mapping.candidateId");
    expect(workflow).toContain("expected_mapping_candidate_id");
    expect(workflow).toContain("expected_mapping_issue_id");
    expect(workflow).toContain("CONFIRMATION_STATUS_UNREVIEWED");
    expect(workflow).toContain("issue_not_auto_registered_unconfirmed");
  });

  it("rechecks the saved summary, raw source, and policy immediately before applying", () => {
    const llm = workflow.indexOf("evaluateSameThreadResolution:");
    const recheck = workflow.indexOf("recheckSourceSummaryAndIssueBeforeApply:");
    const policyRecheck = workflow.indexOf("recheckResolutionPolicyBeforeApply:");
    const apply = workflow.indexOf("applyPreparedDecision:");

    expect(llm).toBeGreaterThan(-1);
    expect(recheck).toBeGreaterThan(llm);
    expect(policyRecheck).toBeGreaterThan(recheck);
    expect(apply).toBeGreaterThan(policyRecheck);
    expect(workflow).toContain("summary-snapshot-changed");
    expect(workflow).toContain("source-snapshot-changed");
    expect(workflow).toContain("policy-snapshot-changed");
    expect(workflow).toContain("expected_revision: $candidate_revision");
    expect(workflow).toContain("expected_issue_revision: $issue_revision");
  });

  it("prepares the three same-thread decisions with valid request payloads", () => {
    const proposed = blockBetween(
      workflow,
      "prepareProposedRejection:",
      "prepareAutoRegisteredCompletion:",
    );
    const completion = blockBetween(
      workflow,
      "prepareAutoRegisteredCompletion:",
      "prepareAutoRegisteredCancellation:",
    );
    const cancellation = blockBetween(
      workflow,
      "prepareAutoRegisteredCancellation:",
      "prepareDecisionDigest:",
    );
    const preparedRequest = blockBetween(
      workflow,
      "prepareResolveCandidateRequest:",
      "recheckSourceSummaryAndIssueBeforeApply:",
    );

    expect(proposed).toContain("CANDIDATE_STATUS_PROPOSED");
    expect(proposed).toContain("reject: {");
    expect(proposed).toContain('reason_category: "RESOLUTION_REASON_CATEGORY_NOT_ACTIONABLE"');
    expect(proposed).not.toContain("confirm_associated_issue:");
    expect(completion).toContain("expected_revision: $issue_revision");
    expect(completion).toContain('target_state: "ISSUE_STATE_DONE"');
    expect(completion).toContain("unverified:");
    expect(completion).toContain("$decision_audit_rationale");
    expect(completion).not.toMatch(/\n\s+verified:/);
    expect(cancellation).toContain("expected_mapping_candidate_id: $candidate_id");
    expect(cancellation).toContain("CANCELLATION_CLASS_FALSE_POSITIVE");
    expect(cancellation).toContain("$decision_audit_rationale");
    expect(preparedRequest).toContain("expected_revision: $candidate_revision");
    expect(preparedRequest).toContain("idempotency_key: $decision_key");
    expect(preparedRequest).not.toContain("$resolution_rationale");
    expect(preparedRequest).not.toContain("$llm_confidence");

    expect(workflow).toContain("reject:");
    expect(workflow).toContain("confirm_associated_issue:");
    expect(workflow).toContain('target_state: "ISSUE_STATE_DONE"');
    expect(workflow).toContain("unverified:");
    expect(workflow).toContain("cancel_associated_issue:");
    expect(workflow).toContain("CANCELLATION_CLASS_FALSE_POSITIVE");
    expect(workflow).toContain("completion: {");
    expect(workflow).toContain("unverified: {");
    expect(workflow).not.toMatch(/\n\s+verified:/);
    expect(workflow).toContain("CANCELLATION_CLASS_FALSE_POSITIVE");
    expect(workflow).toContain("invokeResolveCandidate:");
    expect(workflow).toContain("GetOperation");
    expect(workflow).not.toContain("prepared_decisions");
  });

  it("parses the LLM decision schema and constrains evidence and confidence", () => {
    const schema = resolutionResponseSchema(workflow);
    const properties = schema.properties as Record<
      string,
      {
        enum?: string[];
        minimum?: number;
        maximum?: number;
        minItems?: number;
        uniqueItems?: boolean;
      }
    >;

    expect(schema.required).toEqual(["decision", "rationale", "confidence", "evidence_memory_ids"]);
    expect(properties.decision?.enum).toEqual(["COMPLETE", "CANCEL", "KEEP"]);
    expect(properties.confidence).toMatchObject({ minimum: 0, maximum: 1 });
    expect(properties.evidence_memory_ids).toMatchObject({ minItems: 1, uniqueItems: true });
  });

  it("keeps request identity independent from nondeterministic rationale and confidence", () => {
    const digest = blockBetween(
      workflow,
      "prepareDecisionDigest:",
      "deriveDecisionIdempotencyKey:",
    );

    expect(digest).toContain("source_snapshot_digest: $source_snapshot_digest");
    expect(digest).toContain("decision: $resolution_decision");
    expect(digest).toContain("evidence_memory_ids: $evidence_memory_ids");
    expect(digest).not.toContain("rationale: $resolution_rationale");
    expect(digest).not.toContain("confidence: $llm_confidence");
  });

  it("uses a SHA-256 implementation available on Linux and macOS", () => {
    expect(workflow).toContain("command -v sha256sum");
    expect(workflow).toContain("command -v shasum");
    expect(workflow).not.toContain('args: ["-c", "sha256sum"]');
  });

  it("derives one request key and reports applied, protected, skipped, conflicted, failed, and unconfirmed", () => {
    expect(workflow).toContain("source_snapshot_digest");
    expect(workflow).toContain("resolution_decision");
    expect(workflow).toContain("idempotency_key");
    expect(workflow).toContain("policy_digest");
    expect(workflow).toContain("llm_worker_name");
    expect(workflow).toContain("applied_candidates");
    expect(workflow).toContain("protected_candidates");
    expect(workflow).toContain("skipped_candidates");
    expect(workflow).toContain("conflicted_candidates");
    expect(workflow).toContain("failed_candidates");
    expect(workflow).toContain("unconfirmed_candidates");
    expect(workflow).toContain('application_mode: "APPLY_AFTER_COMMIT"');
    expect(workflow).not.toContain("durable_persistence_required_before_apply");
    expect(workflow).not.toContain("resolution_journal");
  });

  it("calls ResolveCandidate once, then polls the receipt operation with a bounded wait", () => {
    const apply = blockBetween(workflow, "applyPreparedDecision:", "recordUnclassifiedCandidate:");
    const resolveCandidateCalls = workflow.match(/ManagerConsumerService\/ResolveCandidate/g) ?? [];

    expect(resolveCandidateCalls).toHaveLength(1);
    expect(apply).toContain(`json_body: '\${ $resolution_request_payload | tojson }'`);
    expect(apply).toContain("$resolution_response.success.operation_id");
    expect(apply).toContain("ManagerConsumerService/GetOperation");
    expect(apply).toContain("DURABLE_OPERATION_STATE_COMMITTED");
    expect(apply).toContain("markUnresolvedOperationUnconfirmed:");
    expect(apply).toContain("operation_unconfirmed: '$" + "{ true }'");
    expect(apply).toContain("DURABLE_OPERATION_STATE_UNCONFIRMED");
    expect(apply).toContain("RESULT_UNCONFIRMED");
    expect(apply).toContain("operation_poll_attempt");
    expect(apply).toMatch(/range\(0;\s*10\)/);
    expect(apply).toContain("milliseconds: 500");
    expect(apply).not.toContain("deriveDecisionIdempotencyKey");
    expect(apply).not.toContain("decision_key:");
    expect(apply).toContain("applied_candidates");
    expect(apply).toContain("unconfirmed_candidates");
  });

  it("never reports a prepared or non-committed request as applied", () => {
    const applied = blockBetween(workflow, "recordApplied:", "recordUnconfirmed:");
    const applyErrorClassifier = blockBetween(
      workflow,
      "classifyApplyError:",
      "recordApplyConflict:",
    );
    const operationStates = blockBetween(
      workflow,
      "classifyResolutionOperationState:",
      "operation_poll_error_message:",
    );

    expect(workflow).not.toContain("prepared_decisions:");
    expect(workflow).not.toContain("prepared_count:");
    expect(applied).toContain("operation_confirmed == true");
    expect(workflow).toContain('operation_state == "DURABLE_OPERATION_STATE_COMMITTED"');
    expect(workflow).toContain("markUnresolvedOperationUnconfirmed:");
    expect(operationStates).toContain('"DURABLE_OPERATION_STATE_FAILED"');
    expect(operationStates).toContain('"DURABLE_OPERATION_STATE_UNCONFIRMED"');
    expect(workflow).toContain("RESULT_UNCONFIRMED");
    expect(workflow).toContain("recordUnconfirmed:");
    expect(workflow).toContain("recordApplyFailure:");
    expect(workflow).toContain("recordApplyConflict:");
    expect(applyErrorClassifier).toContain('contains("precondition_failed")');
  });

  it("chooses sha256sum or shasum at runtime for portable source digests", () => {
    expect(workflow.match(/if command -v sha256sum/g)).toHaveLength(7);
    expect(workflow).toContain("elif command -v shasum");
    expect(workflow).toContain("shasum -a 256");
    expect(workflow).toContain("else exit 127; fi");
    expect(workflow).not.toContain('args: ["-c", "sha256sum"]');
  });
});
