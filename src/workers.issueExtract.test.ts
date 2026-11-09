import { describe, expect, it } from "vitest";
import threadSummarySingleWorkflow from "../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml?raw";
import extractWorkers from "../workers/lookback-issue-extract-workers.yaml?raw";
import issueExtractWorkflow from "../workers/workflows/issue-extract/issue-extract.yaml?raw";
import periodicWorkflow from "../workers/workflows/lookback-periodic-run.yaml?raw";
import threadSummaryBatchWorkflow from "../workers/workflows/thread-summary/thread-summary-batch.yaml?raw";

function blockBetween(source: string, startMarker: string, endMarker: string): string {
  const start = source.indexOf(startMarker);
  expect(start, `missing ${startMarker}`).toBeGreaterThan(-1);
  const end = source.indexOf(endMarker, start);
  expect(end, `missing ${endMarker}`).toBeGreaterThan(start);
  return source.slice(start, end);
}

describe("issue extract workflow contract", () => {
  it("starts extract only after a persisted thread summary", () => {
    expect(threadSummarySingleWorkflow).toContain("has_unfinished_issue_candidates");
    expect(threadSummarySingleWorkflow).toContain("workerName: lookback-issue-extract");
    const persistIndex = threadSummarySingleWorkflow.indexOf("MEMORY_KIND_THREAD_SUMMARY");
    const extractIndex = threadSummarySingleWorkflow.indexOf("extractIssues:");
    expect(persistIndex).toBeGreaterThan(-1);
    expect(extractIndex).toBeGreaterThan(persistIndex);
    const extractBlock = threadSummarySingleWorkflow.slice(
      extractIndex,
      threadSummarySingleWorkflow.indexOf("- dispatchCrossThreadResolution:"),
    );
    expect(extractBlock).toContain(".extraction_enabled");
    expect(extractBlock).toContain("issue_tracker_grpc_host:");
    expect(extractBlock).toContain("idempotency_key:");
    expect(extractBlock).toContain("now * 1000000");
    expect(extractBlock).not.toMatch(/issue_tracker_grpc_host[^\n]*!= ""/);
    expect(extractBlock).toMatch(/try:[\s\S]*lookback-issue-extract[\s\S]*catch:/);
  });

  it("forwards tracker endpoint and policy through the per-thread batch and periodic workflow", () => {
    expect(threadSummaryBatchWorkflow).toContain("issue_tracker_grpc_host:");
    expect(threadSummaryBatchWorkflow).toContain("policy_json:");
    expect(periodicWorkflow).toContain(
      "issue_tracker_grpc_host: ($workflow.input.runtime.issue_tracker_grpc_host",
    );
    expect(periodicWorkflow).toContain('policy_json: ($workflow.input.runtime.policy_json // "")');
    const dispatch = threadSummaryBatchWorkflow.slice(
      threadSummaryBatchWorkflow.indexOf("- invokeSingleWithRetry:"),
      threadSummaryBatchWorkflow.indexOf(
        "export:\n                    as:\n                      single_result",
      ),
    );
    expect(dispatch).toContain(
      'issue_tracker_grpc_host: ($workflow.input.issue_tracker_grpc_host // "")',
    );
    expect(dispatch).toContain(
      "issue_tracker_grpc_port: ($workflow.input.issue_tracker_grpc_port // 0)",
    );
    expect(dispatch).toContain('policy_json: ($workflow.input.policy_json // "")');
  });

  it("does not launch extract from skip paths", () => {
    for (const reason of [
      "skipped: thread not found",
      "skipped: not enough messages",
      "skipped: already up to date",
    ]) {
      expect(threadSummarySingleWorkflow).toContain(reason);
    }
    expect(threadSummarySingleWorkflow).toMatch(/then: exit[\s\S]*extractIssues:/);
  });

  it("skips LLM and apply when disabled or the summary signal is false", () => {
    expect(issueExtractWorkflow).toContain("required: [thread_id, idempotency_key]");
    expect(issueExtractWorkflow).toContain(".extraction_enabled");
    expect(issueExtractWorkflow).toContain("LOOKBACK_EXPERIMENTAL_ISSUES_ENABLED");
    expect(issueExtractWorkflow).toContain('env.LOOKBACK_EXPERIMENTAL_ISSUES_ENABLED == "true"');
    expect(issueExtractWorkflow).toContain("(try ($workflow.input.policy_json | fromjson)) // {}");
    expect(issueExtractWorkflow).toContain("has_unfinished_issue_candidates");
    expect(issueExtractWorkflow).toContain("skipped: disabled");
    expect(issueExtractWorkflow).not.toContain("skipped: disabled or tracker not ready");
    expect(issueExtractWorkflow).toContain("skipped: no extract signal");
    const skipDisabled = issueExtractWorkflow.indexOf("skipIfDisabled:");
    const skipSignal = issueExtractWorkflow.indexOf("skipIfNoSignal:");
    const skipNotReady = issueExtractWorkflow.indexOf("skipIfNotReady:");
    const generate = issueExtractWorkflow.indexOf("generateCandidates:");
    const apply = issueExtractWorkflow.indexOf("applyExtraction:");
    expect(skipDisabled).toBeGreaterThan(-1);
    expect(skipSignal).toBeGreaterThan(skipDisabled);
    expect(skipNotReady).toBeGreaterThan(skipSignal);
    expect(generate).toBeGreaterThan(skipNotReady);
    expect(apply).toBeGreaterThan(generate);
  });

  it("logs tracker-not-ready as a failure without LLM, apply, or a run", () => {
    const skipNotReady = issueExtractWorkflow.indexOf("skipIfNotReady:");
    const generate = issueExtractWorkflow.indexOf("generateCandidates:");
    const tryBlock = issueExtractWorkflow.indexOf("extractAfterSignal:");
    expect(skipNotReady).toBeGreaterThan(-1);
    expect(skipNotReady).toBeLessThan(tryBlock);
    expect(skipNotReady).toBeLessThan(generate);
    expect(issueExtractWorkflow).toContain("failed: tracker not ready");
    expect(issueExtractWorkflow).toMatch(
      /skipIfNotReady:[\s\S]*result:\s*"failed: tracker not ready"[\s\S]*then:\s*exit[\s\S]*extractAfterSignal:/,
    );
    expect(issueExtractWorkflow).not.toMatch(/skipIfNotReady:[\s\S]*raise:[\s\S]*skipIfNoSignal:/);
    const notReadyBlock = issueExtractWorkflow.slice(skipNotReady, tryBlock);
    expect(notReadyBlock).not.toContain("ApplyExtraction");
    expect(notReadyBlock).not.toContain("workerName:");
  });

  it("keeps only candidates grounded in the supplied message window", () => {
    const generate = issueExtractWorkflow.indexOf("generateCandidates:");
    const shape = issueExtractWorkflow.indexOf("shapeCandidates:");
    const apply = issueExtractWorkflow.indexOf("\n        - applyExtraction:");
    expect(shape).toBeGreaterThan(generate);
    expect(apply).toBeGreaterThan(shape);
    expect(issueExtractWorkflow).toContain("json_corrupt");
    expect(issueExtractWorkflow).toContain("Candidate enum validation and cap ordering are");
    expect(issueExtractWorkflow).toContain("server-owned. Discard only candidates that cannot be");
    expect(issueExtractWorkflow).toContain("locator_memory_ids");
    expect(issueExtractWorkflow).toContain("candidates: ($shaped.candidates // [])");
    expect(issueExtractWorkflow).toMatch(/mixed:\s*(\$shaped\.mixed|\(\$shaped\.mixed)/);
    const shapeBlock = issueExtractWorkflow.slice(shape, apply);
    expect(shapeBlock).toContain("$parsed.candidates | map(");
    expect(shapeBlock).toContain("all(.locator_memory_ids[];");
    expect(shapeBlock).toContain("($window | index($locator)) != null");
    expect(shapeBlock).toContain(".locator_memory_ids | unique | length");
    expect(shapeBlock).toContain("mixed: (($candidates | length) != ($normalized | length))");
    expect(issueExtractWorkflow).not.toContain("adopted_order");
    expect(issueExtractWorkflow).not.toContain("truncated_total");
  });

  it("uses the Liquid JSON encoder available to the completion runner for comparison hints", () => {
    const generate = blockBetween(issueExtractWorkflow, "generateCandidates:", "shapeCandidates:");

    expect(generate).toContain("{{ workflow.input.thread_id }}");
    expect(generate).toContain("{{ workflow.input.output_language }}");
    expect(generate).not.toContain("{{ thread_id }}");
    expect(generate).not.toContain("{{ output_language }}");
    expect(generate).toContain("{{ msg.id.value }}");
    expect(generate).not.toContain("{{ msg.data.id.value }}");
    expect(generate).toContain("{{ hints | json_encode }}");
    expect(generate).not.toContain("{{ hints | tojson }}");
  });

  it("constrains LLM enum output to the IssueTrackerServer candidate contract", () => {
    const generate = blockBetween(issueExtractWorkflow, "generateCandidates:", "shapeCandidates:");

    expect(generate).toContain("kind is TASK, BUG,");
    expect(generate).toContain("RESEARCH, or DECISION");
    expect(generate).toContain("suggested_priority is P0, P1, P2,");
    expect(generate).toContain("P3, or P4");
    expect(generate).toContain("suggested_state is OPEN, IN_PROGRESS, BLOCKED,");
    expect(generate).toContain("or DEFERRED");
    expect(generate).toContain("detection is EXPLICIT or INFERRED");
    expect(generate).toContain(
      '"kind": {"type": "string", "enum": ["TASK", "BUG", "RESEARCH", "DECISION"]}',
    );
    expect(generate).toContain(
      '"suggested_priority": {"type": "string", "enum": ["P0", "P1", "P2", "P3", "P4"]}',
    );
    expect(generate).toContain(
      '"suggested_state": {"type": "string", "enum": ["OPEN", "IN_PROGRESS", "BLOCKED", "DEFERRED"]}',
    );
    expect(generate).toContain('"detection": {"type": "string", "enum": ["EXPLICIT", "INFERRED"]}');
  });

  it("re-fetches source, windows newest-first, and writes a failed run on drift", () => {
    expect(issueExtractWorkflow).toContain('method: "/llm_memory.service.ThreadService/Find"');
    expect(issueExtractWorkflow).toContain("snapshotSourceLabels:");
    expect(issueExtractWorkflow).toContain("FindListByCondition");
    expect(issueExtractWorkflow).toContain('external_id: ("summary:" +');
    expect(issueExtractWorkflow).toContain("shapeWindow:");
    expect(issueExtractWorkflow).toContain("200000");
    expect(issueExtractWorkflow).toContain("refetchMessages:");
    expect(issueExtractWorkflow).toContain("preparePreApplyFailure:");
    expect(issueExtractWorkflow).toContain('then "digest_mismatch"');
    expect(issueExtractWorkflow).toContain("issue_tracker.rpc.v1.IssueExtract/ListExtractionHints");
    expect(issueExtractWorkflow).toContain("issue_tracker.rpc.v1.IssueExtract/ApplyExtraction");
    expect(issueExtractWorkflow).not.toContain("LookbackIssueExtract");
    expect(issueExtractWorkflow).toContain("lookback-issue-extract.v2");
  });

  it("preserves empty messages by attaching their required evidence digest", () => {
    const payload = blockBetween(
      issueExtractWorkflow,
      "prepareApplyMessages:",
      "preparePreApplyFailure:",
    );

    // IssueTrackerServer accepts a raw body or a caller-computed digest. Empty
    // messages remain part of the source snapshot, so payload construction
    // supplies the digest directly instead of mutating source message data.
    expect(payload).toContain("content_digest:");
    expect(payload).toContain('if (.data.content // "") == "" then');
    expect(payload).toContain("sha256:e3b0c44298fc1c149afbf4c8996fb924");
    expect(payload).not.toContain(".data.content_digest");
  });

  it("runs extract on issue_extract, not llm_workflow", () => {
    expect(extractWorkers).toContain("name: lookback-issue-extract");
    expect(extractWorkers).toContain("channel: issue_extract");
    expect(extractWorkers).not.toContain("llm_workflow");
    expect(issueExtractWorkflow).not.toContain("channel: llm_workflow");
  });

  it("builds the shared ApplyExtraction payload once and adds branch-specific fields", () => {
    const sourcePayload = blockBetween(
      issueExtractWorkflow,
      "snapshotSourceLabels:",
      "fetchSummary:",
    );
    const sharedSection = blockBetween(
      issueExtractWorkflow,
      "prepareApplyPayload:",
      "recordFailure:",
    );
    const knownFailure = blockBetween(sharedSection, "applyKnownFailure:", "markApplyStarted:");
    const precondition = blockBetween(
      sharedSection,
      "preparePreApplyFailure:",
      "applyKnownFailure:",
    );
    const normalApply = blockBetween(sharedSection, "applyExtraction:", "failOnApplyError:");

    expect(sharedSection).toMatch(/apply_base_payload:\s*>-/);
    expect(sharedSection).toContain("allowed_messages: $allowed_messages");
    expect(sharedSection).toContain("input_message_ids: $input_message_ids");
    expect(sharedSection.match(/\$apply_base_payload \+ \{/g)).toHaveLength(2);
    expect(knownFailure).toContain("pre_apply_failure_reason");
    expect(precondition).toContain("digest_mismatch");
    expect(precondition).toContain("json_corrupt");
    expect(normalApply).toContain("candidates: ($shaped.candidates // [])");
    expect(normalApply).not.toContain("truncated_total");

    expect(sourcePayload).toContain("policy: $extraction_request_policy");
    expect(sourcePayload).not.toContain("policy_json: $workflow.input.policy_json");
    expect(sourcePayload).toContain("idempotency_key: $workflow.input.idempotency_key");
    expect(sourcePayload).toContain("source_labels: ($source_thread_data.labels // [])");
    expect(sharedSection).not.toContain("omitted_count:");

    const fallbackBlock = issueExtractWorkflow.slice(
      issueExtractWorkflow.indexOf("recordFailure:"),
    );
    expect(fallbackBlock).toContain("($source_labels_ready // false)");
    expect(fallbackBlock).toContain("$source_apply_payload + {");
    expect(fallbackBlock).toContain("allowed_messages: $allowed_messages");
    expect(fallbackBlock).toContain("input_message_ids: $input_message_ids");
    expect(fallbackBlock).not.toContain("$apply_base_payload");
  });

  it("keeps resolution policy out of the IssueTracker extraction protobuf request", () => {
    const init = blockBetween(
      issueExtractWorkflow,
      "prepareExtractionRequestPolicy:",
      "- skipIfDisabled:",
    );
    const payload = blockBetween(issueExtractWorkflow, "snapshotSourceLabels:", "fetchSummary:");

    expect(init).toContain("extraction_request_policy:");
    expect(init).toContain('if type == "object"');
    expect(init).not.toContain("resolution");
    expect(init).toContain("else {}");
    expect(payload).toContain("policy: $extraction_request_policy");
    expect(payload).not.toContain("policy: $extraction_policy");
  });

  it("turns an empty gRPC response into an ApplyExtraction error instead of parsing null", () => {
    const sharedSection = blockBetween(
      issueExtractWorkflow,
      "prepareApplyPayload:",
      "failOnApplyError:",
    );
    const fallbackBlock = issueExtractWorkflow.slice(
      issueExtractWorkflow.indexOf("recordFailure:"),
    );

    for (const block of [sharedSection, fallbackBlock]) {
      expect(block).toContain('if (.jsonBody | type) == "string" then');
      expect(block).toContain('code: "grpc_transport"');
      expect(block).toContain('.message // "ApplyExtraction returned no response body"');
    }
    expect(sharedSection).not.toMatch(/extract_output: \$\{ \.jsonBody \| fromjson \}/);
    expect(fallbackBlock).not.toMatch(/extract_output: \$\{ \.jsonBody \| fromjson \}/);
  });

  it("forwards an explicit rejected-candidate suppression override only through a single-thread extraction", () => {
    const extractInput = blockBetween(issueExtractWorkflow, "input:", "do:");
    const singleInput = blockBetween(threadSummarySingleWorkflow, "input:", "do:");
    const extractBlock = threadSummarySingleWorkflow.slice(
      threadSummarySingleWorkflow.indexOf("extractIssues:"),
      threadSummarySingleWorkflow.indexOf("\noutput:"),
    );

    expect(extractInput).toContain("rejected_candidate_ids_to_unsuppress");
    expect(issueExtractWorkflow).toContain("rejected_candidate_ids_to_unsuppress");
    expect(singleInput).toContain("rejected_candidate_ids_to_unsuppress");
    expect(extractBlock).toContain("rejected_candidate_ids_to_unsuppress");

    // The batch accepts a per-thread override, but emits it only for the
    // explicitly targeted one-thread dispatch. Import and periodic paths do
    // not forward the map, so they retain the child default (an empty array).
    const batchInput = blockBetween(threadSummaryBatchWorkflow, "input:", "\ndo:");
    const invokeSingle = blockBetween(
      threadSummaryBatchWorkflow,
      "invokeSingleWithRetry:",
      "export:\n                    as:\n                      single_result",
    );
    expect(batchInput).toContain("rejected_candidate_ids_to_unsuppress_by_thread");
    expect(invokeSingle).toContain("target_thread_ids | length) == 1");
    expect(invokeSingle).toContain("as $rejected_by_thread");
    expect(invokeSingle).toContain("$rejected_by_thread[$thread_id]");
    expect(invokeSingle).toContain("rejected_candidate_ids_to_unsuppress:");
    expect(periodicWorkflow).not.toContain("rejected_candidate_ids_to_unsuppress");
  });

  it("retries a known FAILED write, but never a second Apply after candidate Apply starts", () => {
    const knownFailure = issueExtractWorkflow.indexOf("applyKnownFailure:");
    const failureStarted = issueExtractWorkflow.indexOf("apply_started: true", knownFailure);
    const mark = issueExtractWorkflow.indexOf("markApplyStarted:");
    const apply = issueExtractWorkflow.indexOf("\n        - applyExtraction:");
    const catchBlock = issueExtractWorkflow.slice(issueExtractWorkflow.indexOf("catch:"));
    expect(failureStarted).toBeGreaterThan(knownFailure);
    expect(mark).toBeGreaterThan(failureStarted);
    expect(apply).toBeGreaterThan(mark);
    expect(catchBlock).toContain("recordFailure:");
    expect(catchBlock).toContain("($apply_started // false) | not");
  });

  it("normalizes the extraction policy once before every workflow branch", () => {
    const init = blockBetween(issueExtractWorkflow, "- init:", "prepareExtractionRequestPolicy:");
    const skip = blockBetween(issueExtractWorkflow, "- skipIfDisabled:", "- skipIfNoSignal:");
    expect(init).toContain("extraction_policy:");
    expect(init).toContain("(try ($workflow.input.policy_json | fromjson)) // {}");
    expect(skip).toContain("$extraction_policy.extraction_enabled");
    expect(issueExtractWorkflow.match(/policy: \$extraction_request_policy/g)).toHaveLength(1);
  });

  it("fails the workflow when ApplyExtraction returns its error outcome", () => {
    const apply = issueExtractWorkflow.indexOf("\n        - applyExtraction:");
    const errorCheck = issueExtractWorkflow.indexOf("failOnApplyError:");
    const catchBlock = issueExtractWorkflow.slice(issueExtractWorkflow.indexOf("catch:"));

    expect(errorCheck).toBeGreaterThan(apply);
    expect(
      issueExtractWorkflow.slice(errorCheck, issueExtractWorkflow.indexOf("catch:")),
    ).toContain("($extract_output.error // null) != null");
    expect(
      issueExtractWorkflow.slice(errorCheck, issueExtractWorkflow.indexOf("catch:")),
    ).toContain("$extract_output.error.grpc_code");
    expect(catchBlock).toContain("rethrowExtractionError:");
  });

  it("evaluates caught and ApplyExtraction failures before it rethrows them", () => {
    const applyErrorBlock = blockBetween(issueExtractWorkflow, "failOnApplyError:", "catch:");
    const catchBlock = issueExtractWorkflow.slice(issueExtractWorkflow.indexOf("catch:"));

    expect(applyErrorBlock).toContain("detail: >-");
    expect(applyErrorBlock).toContain("$extract_output.error.message");
    expect(catchBlock).toContain("detail: >-");
    expect(catchBlock).toContain("$extractError.detail");
    expect(catchBlock).toContain("($extractError | tostring)");
    const legacyCatchDetail = [
      'detail: "',
      "${",
      ' $extractError.detail // $extractError.title // \\"Issue extraction failed\\" }"',
    ].join("");
    expect(issueExtractWorkflow).not.toContain(legacyCatchDetail);
  });
});
