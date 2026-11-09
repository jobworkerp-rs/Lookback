import { spawnSync } from "node:child_process";
import { describe, expect, it } from "vitest";
import workflow from "../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml?raw";
import batchWorkflow from "../workers/workflows/thread-summary/thread-summary-batch.yaml?raw";

const jqAvailable = spawnSync("jq", ["--version"], { encoding: "utf8" }).status === 0;

function blockBetween(source: string, startMarker: string, endMarker: string): string {
  const start = source.indexOf(startMarker);
  expect(start, `missing ${startMarker}`).toBeGreaterThan(-1);
  const end = source.indexOf(endMarker, start);
  expect(end, `missing ${endMarker}`).toBeGreaterThan(start);
  return source.slice(start, end);
}

function expressionUntil(source: string, field: string, nextMarker: string): string {
  const fieldMarker = source.indexOf(`${field}: >-`);
  expect(fieldMarker, `missing ${field}`).toBeGreaterThan(-1);
  const expressionStart = source.indexOf("${", fieldMarker);
  expect(expressionStart, `missing expression for ${field}`).toBeGreaterThan(fieldMarker);
  const nextIndex = source.indexOf(nextMarker, expressionStart);
  expect(nextIndex, `missing end marker ${nextMarker}`).toBeGreaterThan(expressionStart);
  const fieldBody = source.slice(expressionStart + 2, nextIndex);
  const expressionEnd = fieldBody.lastIndexOf("}");
  expect(expressionEnd, `missing expression end for ${field}`).toBeGreaterThan(0);
  return fieldBody.slice(0, expressionEnd).trim();
}

function evaluateJq(
  expression: string,
  variables: Record<string, unknown>,
  experimentalIssuesEnabled = true,
): unknown {
  const args = ["-c", "-n"];
  for (const [name, value] of Object.entries(variables)) {
    args.push("--argjson", name, JSON.stringify(value));
  }
  args.push(expression);

  const result = spawnSync("jq", args, {
    encoding: "utf8",
    env: {
      ...process.env,
      LOOKBACK_EXPERIMENTAL_ISSUES_ENABLED: experimentalIssuesEnabled ? "true" : "false",
    },
  });
  expect(result.error).toBeUndefined();
  expect(result.status, `${result.stderr}\n${expression}`).toBe(0);
  return JSON.parse(result.stdout.trim());
}

function evaluateResolutionError(workerOutput: Record<string, unknown>): unknown {
  const expression = expressionUntil(
    blockBetween(workflow, "- resolveThreadIssueCandidates:", "- extractIssues:"),
    "resolution_error",
    "- waitBeforeFirstResolutionRetry:",
  );

  return evaluateJq(expression, { resolution_outcome: workerOutput });
}

function advancementVariables(overrides: Record<string, unknown> = {}) {
  return {
    existing_summary_memory: { id: { value: "saved-summary" } },
    previous_message_checkpoint_valid: true,
    current_message_checkpoint_valid: true,
    current_message_position: 2,
    previous_message_position: 1,
    current_message_at: 300,
    previous_message_at: 200,
    resolution_mode: "APPLY",
    previous_resolution_high_water_absent: true,
    previous_resolution_high_water_id: "",
    previous_resolution_high_water_at: null,
    summary_json: { has_unfinished_issue_candidates: false },
    workflow: {
      input: {
        force_resummarize: false,
        policy_json: JSON.stringify({ extraction_enabled: false }),
      },
    },
    ...overrides,
  };
}

describe("single-thread summary same-thread issue dispatch", () => {
  it("dispatches only for an advancing APPLY checkpoint, independently of extraction", () => {
    expect(jqAvailable, "jq is required to execute workflow expression fixtures").toBe(true);

    const advancement = expressionUntil(
      workflow,
      "resolution_high_water_should_advance",
      "- prepareSummaryMetadata:",
    );
    const dispatch = blockBetween(workflow, "- resolveThreadIssueCandidates:", "- extractIssues:");

    expect(
      evaluateJq(advancement, advancementVariables()),
      "new source messages must dispatch even when extraction is disabled and the summary has no unfinished-issue signal",
    ).toBe(true);
    expect(evaluateJq(advancement, advancementVariables(), false)).toBe(false);

    for (const variables of [
      advancementVariables({
        existing_summary_memory: null,
        previous_message_checkpoint_valid: false,
      }),
      advancementVariables({ previous_message_checkpoint_valid: false }),
      advancementVariables({ workflow: { input: { force_resummarize: true } } }),
      advancementVariables({ current_message_position: 1, current_message_at: 200 }),
      advancementVariables({ resolution_mode: "DISABLED" }),
    ]) {
      expect(evaluateJq(advancement, variables)).toBe(false);
    }

    expect(dispatch).toContain(`if: "\${ $resolution_high_water_should_advance }"`);
    expect(dispatch).not.toContain("extraction_enabled");
    expect(dispatch).not.toContain("has_unfinished_issue_candidates");
    expect(workflow.indexOf("- skipIfNotEligible:")).toBeLessThan(
      workflow.indexOf("- resolveThreadIssueCandidates:"),
    );
    expect(blockBetween(workflow, "- skipIfNotEligible:", "- truncateMessages:")).toContain(
      "then: exit",
    );
    expect(workflow.indexOf("- updateSummary:")).toBeLessThan(
      workflow.indexOf("- resolveThreadIssueCandidates:"),
    );
    expect(workflow.indexOf("- addSummaryMemory:")).toBeLessThan(
      workflow.indexOf("- resolveThreadIssueCandidates:"),
    );
  });

  it("passes the persisted summary snapshot, checkpoint, endpoints, policy, and LLM settings", () => {
    const dispatch = blockBetween(workflow, "- resolveThreadIssueCandidates:", "- extractIssues:");

    expect(dispatch).toContain("workerName: lookback-thread-issue-resolution");
    expect(dispatch).toContain("summary_memory_id: $saved_summary_memory_id");
    expect(dispatch).toContain(`stdin: "\${ $summary_json | tojson }"`);
    expect(dispatch).toContain("summary_checkpoint_memory_id: ($current_message_id | tostring)");
    expect(dispatch).toContain("summary_checkpoint_at: ($current_message_at | tostring)");
    expect(dispatch).toContain("memories_grpc_host: $workflow.input.memories_grpc_host");
    expect(dispatch).toContain("memories_grpc_port: $workflow.input.memories_grpc_port");
    expect(dispatch).toContain(
      'issue_tracker_grpc_host: ($workflow.input.issue_tracker_grpc_host // "")',
    );
    expect(dispatch).toContain(
      "issue_tracker_grpc_port: ($workflow.input.issue_tracker_grpc_port // 0)",
    );
    expect(dispatch).toContain("llm_worker_name: $workflow.input.llm_worker_name");
    expect(dispatch).toContain('policy_json: ($workflow.input.policy_json // "")');
    expect(workflow).toContain("add_summary_memory_response.id.value");
  });

  it("isolates incomplete, unconfirmed, and thrown resolution results from summary success", () => {
    const dispatch = blockBetween(workflow, "- resolveThreadIssueCandidates:", "- extractIssues:");
    const catchBlock = dispatch.slice(dispatch.indexOf("catch:"));

    expect(dispatch).toContain("resolution_outcome:");
    expect(dispatch).toContain("unconfirmed_count");
    expect(dispatch).toContain("resolution_complete");
    expect(catchBlock).toContain("resolution_error:");
    expect(catchBlock).not.toContain("summary_json:");
    expect(catchBlock).not.toContain("then: exit");
    expect(workflow).toContain(`completed: \${ $summary_json != null }`);

    const completeExpression = expressionUntil(
      blockBetween(dispatch, "- classifyResolutionAttempt:", "- recordCompleteResolution:"),
      "resolution_complete",
      "resolution_retry_allowed:",
    );
    expect(
      evaluateJq(completeExpression, {
        resolution_outcome: { complete: true, failed_count: 0, unconfirmed_count: 0 },
      }),
    ).toBe(true);
    expect(
      evaluateResolutionError({ complete: false, failed_count: 0, unconfirmed_count: 1 }),
    ).not.toBeNull();
    expect(
      evaluateJq(completeExpression, {
        resolution_outcome: { complete: true, failed_count: 0, unconfirmed_count: 1 },
      }),
    ).toBe(false);
  });

  it("persists PENDING with an eligible summary and preserves the last status otherwise", () => {
    expect(jqAvailable, "jq is required to execute workflow expression fixtures").toBe(true);

    const metadataExpression = expressionUntil(
      workflow,
      "        summary_metadata",
      "\n        resolution_status:",
    );
    const metadataFor = (shouldAdvance: boolean, priorStatus?: string) => {
      const existingMetadata = {
        custom_field: "preserved",
        ...(priorStatus == null ? {} : { issue_resolution_status: priorStatus }),
      };
      const serialized = evaluateJq(
        metadataExpression,
        advancementVariables({
          existing_summary_metadata: existingMetadata,
          workflow: { input: { thread_id: "thread-1", force_resummarize: false } },
          summary_json: { category: "general" },
          processed_message_checkpoint_should_advance: true,
          current_message_id: "message-3",
          current_message_at: 300,
          resolution_high_water_should_advance: shouldAdvance,
        }),
      );

      return JSON.parse(serialized as string) as Record<string, unknown>;
    };

    expect(metadataFor(true, "COMPLETE")).toMatchObject({
      custom_field: "preserved",
      issue_resolution_status: "PENDING",
    });
    expect(metadataFor(false, "UNKNOWN")).toMatchObject({
      custom_field: "preserved",
      issue_resolution_status: "UNKNOWN",
    });
    expect(metadataFor(false)).not.toHaveProperty("issue_resolution_status");
  });

  it("keeps a crash-visible PENDING status without replaying it on an unchanged checkpoint", () => {
    const metadataExpression = expressionUntil(
      workflow,
      "        summary_metadata",
      "\n        resolution_status:",
    );
    const pendingMetadata = evaluateJq(
      metadataExpression,
      advancementVariables({
        existing_summary_metadata: { issue_resolution_status: "UNKNOWN" },
        workflow: { input: { thread_id: "thread-1", force_resummarize: false } },
        summary_json: { category: "general" },
        processed_message_checkpoint_should_advance: true,
        current_message_id: "message-3",
        current_message_at: 300,
        resolution_high_water_should_advance: true,
      }),
    );
    const pendingMemoryMetadata = JSON.parse(pendingMetadata as string) as Record<string, unknown>;
    const advancement = expressionUntil(
      workflow,
      "resolution_high_water_should_advance",
      "- prepareSummaryMetadata:",
    );
    const dispatch = blockBetween(workflow, "- resolveThreadIssueCandidates:", "- extractIssues:");
    const dispatchGate = dispatch.slice(0, dispatch.indexOf("try:"));

    expect(pendingMemoryMetadata.issue_resolution_status).toBe("PENDING");
    expect(dispatch).toContain(`if: "\${ $resolution_high_water_should_advance }"`);
    expect(dispatchGate).not.toContain("issue_resolution_status");
    expect(
      evaluateJq(
        advancement,
        advancementVariables({
          current_message_position: 1,
          previous_message_position: 1,
          current_message_at: 300,
          previous_message_at: 300,
          previous_resolution_high_water_absent: false,
          previous_resolution_high_water_id: "message-3",
          previous_resolution_high_water_at: 300,
        }),
      ),
    ).toBe(false);
  });

  it("retries only bounded transient failures that are known to precede mutation", () => {
    expect(jqAvailable, "jq is required to execute workflow expression fixtures").toBe(true);

    const retryExpression = expressionUntil(
      workflow,
      "resolution_retry_allowed",
      "- recordCompleteResolution:",
    );
    const retryAllowed = (outcome: Record<string, unknown>, attempt: number) =>
      evaluateJq(retryExpression, {
        resolution_outcome: outcome,
        resolution_attempt_index: attempt,
      });

    expect(
      retryAllowed(
        {
          complete: false,
          result: "failed: tracker not ready",
          failed_count: 0,
          unconfirmed_count: 0,
          failed_candidates: [],
        },
        0,
      ),
    ).toBe(true);
    expect(
      retryAllowed(
        {
          complete: false,
          result: "failed: tracker not ready",
          failed_count: 0,
          unconfirmed_count: 0,
          failed_candidates: [],
        },
        2,
      ),
    ).toBe(false);
    expect(
      retryAllowed(
        {
          complete: false,
          failed_count: 1,
          unconfirmed_count: 0,
          applied_count: 1,
          conflicted_count: 0,
          failed_candidates: [{ source: "resolution_llm", reason: "worker unavailable" }],
        },
        0,
      ),
    ).toBe(false);
    expect(
      retryAllowed(
        {
          complete: false,
          failed_count: 1,
          unconfirmed_count: 0,
          failed_candidates: [{ source: "resolution_llm", reason: "worker unavailable" }],
        },
        0,
      ),
    ).toBe(true);
    expect(
      retryAllowed(
        {
          complete: false,
          failed_count: 1,
          unconfirmed_count: 1,
          failed_candidates: [{ source: "resolution_llm", reason: "worker unavailable" }],
        },
        0,
      ),
    ).toBe(false);
    expect(
      retryAllowed(
        {
          complete: false,
          failed_count: 1,
          unconfirmed_count: 0,
          failed_candidates: [{ source: "tracker", reason: "unavailable" }],
        },
        0,
      ),
    ).toBe(false);
    expect(
      retryAllowed(
        {
          complete: false,
          failed_count: 1,
          unconfirmed_count: 0,
          failed_candidates: [{ source: "resolution_llm", reason: "invalid response" }],
        },
        0,
      ),
    ).toBe(false);
    expect(
      retryAllowed(
        {
          complete: false,
          failed_count: 1,
          unconfirmed_count: 0,
          failed_candidates: [{ source: "resolution_llm", reason: "worker unavailable" }],
        },
        2,
      ),
    ).toBe(false);

    expect(workflow).toContain(`in: '\${ [range(0; 3)] }'`);
    expect(workflow).toContain("wait: { seconds: 1 }");
    expect(workflow).toContain("wait: { seconds: 2 }");
  });

  it("re-reads the saved summary and updates only its resolution status", () => {
    expect(jqAvailable, "jq is required to execute workflow expression fixtures").toBe(true);

    const statusUpdate = blockBetween(
      workflow,
      "- finalizeThreadIssueResolution:",
      "- extractIssues:",
    );
    const metadataExpression = expressionUntil(
      statusUpdate,
      "resolution_status_metadata",
      "- persistResolutionStatus:",
    );

    expect(statusUpdate).toContain("/llm_memory.service.MemoryService/Find");
    expect(statusUpdate).toContain("/llm_memory.service.MemoryService/Update");
    expect(statusUpdate).toContain("resolution_summary_matches_run");
    expect(statusUpdate).toContain("data: ($resolution_summary_memory.data");
    expect(statusUpdate).toContain(".metadata = $resolution_status_metadata");

    const snapshotExpression = expressionUntil(
      statusUpdate,
      "resolution_summary_matches_run",
      "- reportChangedResolutionSummary:",
    );
    const summaryJson = { category: "general" };
    const snapshotVariables = {
      resolution_summary_memory: {
        id: { value: "saved-summary" },
        data: { content: JSON.stringify(summaryJson) },
      },
      resolution_summary_metadata: {
        source_thread_id: "thread-1",
        resolution_high_water_message_id: "message-3",
        resolution_high_water_message_at: 300,
        issue_resolution_status: "PENDING",
      },
      saved_summary_memory_id: "saved-summary",
      summary_json: summaryJson,
      workflow: { input: { thread_id: "thread-1" } },
      current_message_id: "message-3",
      current_message_at: 300,
    };
    expect(evaluateJq(snapshotExpression, snapshotVariables)).toBe(true);
    expect(
      evaluateJq(snapshotExpression, {
        ...snapshotVariables,
        resolution_summary_memory: {
          ...snapshotVariables.resolution_summary_memory,
          data: { content: JSON.stringify({ category: "changed" }) },
        },
      }),
    ).toBe(false);

    for (const resolutionStatus of ["COMPLETE", "UNKNOWN"]) {
      const serialized = evaluateJq(metadataExpression, {
        resolution_summary_metadata: {
          custom_field: "preserved",
          issue_resolution_status: "PENDING",
          last_processed_message_id: "message-3",
        },
        resolution_status: resolutionStatus,
      });
      expect(JSON.parse(serialized as string)).toEqual({
        custom_field: "preserved",
        issue_resolution_status: resolutionStatus,
        last_processed_message_id: "message-3",
      });
    }
    expect(statusUpdate.indexOf("MemoryService/Find")).toBeLessThan(
      statusUpdate.indexOf("MemoryService/Update"),
    );
  });
});

describe("thread summary batch resolution outcomes", () => {
  it("aggregates resolution outcomes and errors without turning them into summary failures", () => {
    const errorStep = blockBetween(
      batchWorkflow,
      "- recordIssueResolutionError:",
      "- recordIssueResolutionOutcome:",
    );
    const outcomeStep = blockBetween(batchWorkflow, "- recordIssueResolutionOutcome:", "catch:");

    expect(batchWorkflow).toContain(`issue_resolution_errors: "\${ [] }"`);
    expect(batchWorkflow).toContain(`issue_resolution_outcomes: "\${ [] }"`);
    expect(batchWorkflow).toContain(`issue_resolution_errors: \${ $issue_resolution_errors }`);
    expect(batchWorkflow).toContain(`issue_resolution_outcomes: \${ $issue_resolution_outcomes }`);
    expect(errorStep).toContain("single_result.resolution_error");
    expect(outcomeStep).toContain("single_result.resolution_status");
    expect(outcomeStep).toContain("single_result.resolution_outcome");
    expect(errorStep).not.toContain("failed_threads:");
    expect(outcomeStep).not.toContain("failed_threads:");
    expect(batchWorkflow).toContain("or ($single_result.skipped // false)");

    const errorExpression = expressionUntil(
      batchWorkflow,
      "issue_resolution_errors",
      "- recordIssueResolutionOutcome:",
    );
    const outcomeExpression = expressionUntil(
      batchWorkflow,
      "issue_resolution_outcomes",
      "\n            catch:",
    );
    expect(
      evaluateJq(errorExpression, {
        issue_resolution_errors: [],
        single_result: { resolution_error: "tracker unavailable", resolution_status: "UNKNOWN" },
        thread: { id: { value: "thread-1" } },
      }),
    ).toEqual([{ thread_id: "thread-1", status: "UNKNOWN", message: "tracker unavailable" }]);
    expect(
      evaluateJq(outcomeExpression, {
        issue_resolution_outcomes: [],
        single_result: {
          resolution_status: "PENDING",
          resolution_complete: false,
          resolution_outcome: null,
        },
        thread: { id: { value: "thread-1" } },
      }),
    ).toEqual([
      {
        thread_id: "thread-1",
        status: "PENDING",
        complete: false,
        outcome: null,
      },
    ]);
  });
});
