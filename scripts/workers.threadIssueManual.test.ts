import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { describe, expect, it } from "vitest";
import extractWorkers from "../workers/lookback-issue-extract-workers.yaml?raw";
import workflow from "../workers/workflows/thread-issue-resolution/thread-issue-resolution-manual.yaml?raw";

const jqAvailable = spawnSync("jq", ["--version"], { encoding: "utf8" }).status === 0;

function blockBetween(source: string, startMarker: string, endMarker: string): string {
  const start = source.indexOf(startMarker);
  expect(start, `missing ${startMarker}`).toBeGreaterThan(-1);
  const end = source.indexOf(endMarker, start);
  expect(end, `missing ${endMarker}`).toBeGreaterThan(start);
  return source.slice(start, end);
}

function expressionIn(block: string, field: string): string {
  const fieldIndex = block.indexOf(field);
  expect(fieldIndex, `missing ${field}`).toBeGreaterThan(-1);
  const start = block.indexOf("${", fieldIndex);
  expect(start, `missing expression for ${field}`).toBeGreaterThan(fieldIndex);
  let depth = 1;
  let inString = false;
  let escaped = false;
  for (let index = start + 2; index < block.length; index += 1) {
    const character = block[index];
    if (inString) {
      if (escaped) {
        escaped = false;
      } else if (character === "\\") {
        escaped = true;
      } else if (character === '"') {
        inString = false;
      }
      continue;
    }
    if (character === '"') {
      inString = true;
    } else if (character === "{") {
      depth += 1;
    } else if (character === "}") {
      depth -= 1;
      if (depth === 0) return block.slice(start + 2, index).trim();
    }
  }
  throw new Error(`missing expression end for ${field}`);
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
  return JSON.parse(result.stdout.trim()) as unknown;
}

function validSummaryFixture() {
  const summary = {
    id: { value: "summary-memory-1" },
    data: {
      externalId: "summary:thread-1",
      memoryKind: "MEMORY_KIND_THREAD_SUMMARY",
      contentType: "CONTENT_TYPE_TEXT",
      content: "saved summary body",
      metadata: JSON.stringify({
        source_thread_id: "thread-1",
        resolution_high_water_message_id: "message-9",
        resolution_high_water_message_at: 1_700_000_000_000,
      }),
    },
  };
  const metadata = JSON.parse(summary.data.metadata) as Record<string, unknown>;
  return {
    summary,
    variables: {
      summary_entries: [summary],
      saved_summary: summary,
      summary_memory_id: "summary-memory-1",
      summary_external_id: "summary:thread-1",
      summary_kind: "MEMORY_KIND_THREAD_SUMMARY",
      summary_content_type: "CONTENT_TYPE_TEXT",
      summary_content: "saved summary body",
      summary_metadata: metadata,
      summary_checkpoint_memory_id: "message-9",
      summary_checkpoint_timestamp: 1_700_000_000_000,
      workflow: { input: { thread_id: "thread-1" } },
    },
  };
}

describe("manual same-thread issue resolution workflow", () => {
  it("falls back to the summary's processed checkpoint when no resolution high-water mark exists", () => {
    expect(jqAvailable, "jq is required to execute workflow expression fixtures").toBe(true);
    // The checkpoint pair is selected once per summary; every field reads it.
    const selectCheckpoint = expressionIn(
      blockBetween(workflow, "- selectSummaryCheckpoint:", "- extractSavedSummarySnapshot:"),
      "summary_checkpoint:",
    );
    const extractSnapshot = blockBetween(
      workflow,
      "- extractSavedSummarySnapshot:",
      "- requireSavedSummary:",
    );
    const field = (key: string) => {
      const expression = expressionIn(extractSnapshot, `${key}:`);
      return (summary_metadata: Record<string, unknown>) =>
        evaluateJq(expression, {
          summary_checkpoint: evaluateJq(selectCheckpoint, { summary_metadata }),
        });
    };
    const checkpointId = field("summary_checkpoint_memory_id");
    const checkpointAt = field("summary_checkpoint_at");
    const checkpointTimestamp = field("summary_checkpoint_timestamp");
    const checkpointSource = field("summary_checkpoint_source");
    const requireCondition = expressionIn(
      blockBetween(workflow, "- requireSavedSummary:", "- digestSavedSummary:"),
      "if:",
    );
    const fixture = validSummaryFixture();
    const evaluateRequire = (metadata: Record<string, unknown>) => {
      const summary_metadata = { source_thread_id: "thread-1", ...metadata };
      return evaluateJq(requireCondition, {
        ...fixture.variables,
        summary_metadata,
        summary_checkpoint_memory_id: checkpointId(summary_metadata),
        summary_checkpoint_timestamp: checkpointTimestamp(summary_metadata),
      });
    };

    // Summaries that were never auto-dispatched only carry the processed
    // message checkpoint; the manual path must still be able to use them.
    const processedOnly = {
      last_processed_message_id: "message-7",
      last_processed_message_at: 1_600_000_000_000,
      resolution_high_water_message_id: null,
      resolution_high_water_message_at: null,
    };
    expect(checkpointId(processedOnly)).toBe("message-7");
    expect(checkpointAt(processedOnly)).toBe("1600000000000");
    expect(checkpointTimestamp(processedOnly)).toBe(1_600_000_000_000);
    expect(checkpointSource(processedOnly)).toBe("last_processed");
    expect(evaluateRequire(processedOnly)).toBe(false);

    const both = {
      ...processedOnly,
      resolution_high_water_message_id: "message-9",
      resolution_high_water_message_at: 1_700_000_000_000,
    };
    expect(checkpointId(both)).toBe("message-9");
    expect(checkpointSource(both)).toBe("resolution_high_water");
    expect(evaluateRequire(both)).toBe(false);

    for (const incomplete of [
      {},
      { last_processed_message_id: "message-7" },
      { last_processed_message_at: 1_600_000_000_000 },
      { resolution_high_water_message_id: "message-9" },
      { ...processedOnly, last_processed_message_at: "not-a-time" },
    ]) {
      expect(checkpointSource(incomplete), JSON.stringify(incomplete)).toBe(
        checkpointId(incomplete) === "" ? "" : "last_processed",
      );
      expect(evaluateRequire(incomplete), JSON.stringify(incomplete)).toBe(true);
    }

    // The status write re-reads the summary and must select the same pair.
    const selectStatusCheckpoint = expressionIn(
      blockBetween(workflow, "- selectCurrentSummaryCheckpoint:", "- digestCurrentSummaryContent:"),
      "status_summary_checkpoint:",
    );
    const normalized = (expression: string) => expression.replace(/\s+/g, " ");
    expect(normalized(selectStatusCheckpoint.replace("$status_summary_metadata", "$summary_metadata"))).toBe(
      normalized(selectCheckpoint),
    );
    const statusSnapshot = blockBetween(
      workflow,
      "- extractCurrentSummarySnapshot:",
      "- verifyStatusSummarySnapshot:",
    );
    const statusMetadata = { source_thread_id: "thread-1", ...processedOnly };
    const status_summary_checkpoint = evaluateJq(selectStatusCheckpoint, {
      status_summary_metadata: statusMetadata,
    });
    expect(
      evaluateJq(expressionIn(statusSnapshot, "status_summary_checkpoint_memory_id:"), { status_summary_checkpoint }),
    ).toBe("message-7");
    expect(
      evaluateJq(expressionIn(statusSnapshot, "status_summary_checkpoint_at:"), { status_summary_checkpoint }),
    ).toBe("1600000000000");
    const matchesSnapshot = expressionIn(
      blockBetween(workflow, "- verifyStatusSummarySnapshot:", "- skipSummaryStatusWriteOnSnapshotChange:"),
      "status_summary_matches_snapshot:",
    );
    expect(
      evaluateJq(matchesSnapshot, {
        status_summary: { id: { value: "summary-memory-1" } },
        status_summary_metadata: statusMetadata,
        status_summary_memory_id: "summary-memory-1",
        status_summary_external_id: "summary:thread-1",
        status_summary_source_thread_id: "thread-1",
        status_summary_content_digest: "sha256:body-digest",
        summary_content_digest: "sha256:body-digest",
        status_summary_checkpoint_memory_id: "message-7",
        summary_checkpoint_memory_id: "message-7",
        status_summary_checkpoint_at: "1600000000000",
        summary_checkpoint_at: "1600000000000",
        summary_memory_id: "summary-memory-1",
        workflow: { input: { thread_id: "thread-1" } },
      }),
    ).toBe(true);
    expect(workflow).toMatch(/output:[\s\S]*checkpoint_source: '\$\{ \$summary_checkpoint_source \}'/);
  });

  it("separates the waiting manual parent from the issue_extract child channel", () => {
    const childRegistrationStart = extractWorkers.indexOf(
      "  - name: lookback-thread-issue-resolution\n",
    );
    expect(childRegistrationStart).toBeGreaterThan(-1);
    const childRegistrationEnd = extractWorkers.indexOf("\n  - name:", childRegistrationStart + 1);
    const childRegistration = extractWorkers.slice(
      childRegistrationStart,
      childRegistrationEnd === -1 ? undefined : childRegistrationEnd,
    );
    expect(childRegistration).toContain("channel: issue_extract");
    expect(childRegistration).toContain(
      "$file: workflows/thread-issue-resolution/thread-issue-resolution.yaml",
    );

    const registrationStart = extractWorkers.indexOf(
      "  - name: lookback-thread-issue-resolution-manual",
    );
    expect(registrationStart).toBeGreaterThan(-1);
    const nextRegistration = extractWorkers.indexOf("\n  - name:", registrationStart + 1);
    const registration = extractWorkers.slice(
      registrationStart,
      nextRegistration === -1 ? undefined : nextRegistration,
    );

    expect(registration).toContain("channel: issue_resolution_manual");
    expect(registration).toContain(
      "$file: workflows/thread-issue-resolution/thread-issue-resolution-manual.yaml",
    );
    expect(workflow).toContain("name: thread-issue-resolution-manual");
    expect(workflow).toContain("lookback-thread-issue-resolution");
    expect(workflow).not.toContain("for:");
    expect(workflow).not.toContain("wait:");
  });

  it("fails closed before lookup or child dispatch unless the policy is APPLY", () => {
    expect(jqAvailable, "jq is required to execute workflow expression fixtures").toBe(true);
    const policyInit = blockBetween(
      workflow,
      "- initializeManualResolution:",
      "- normalizeManualPolicy:",
    );
    const policyExpression = expressionIn(policyInit, "policy:");
    const normalizedPolicy = blockBetween(
      workflow,
      "- normalizeManualPolicy:",
      "- skipIfDisabled:",
    );
    const resolutionModeExpression = expressionIn(normalizedPolicy, "resolution_mode:");
    const modeFor = (policyJson: string) => {
      const policy = evaluateJq(policyExpression, {
        workflow: { input: { policy_json: policyJson } },
      });
      return evaluateJq(resolutionModeExpression, { policy });
    };

    expect(modeFor('{"resolution_mode":"APPLY"}')).toBe("APPLY");
    expect(modeFor('{"resolution_mode":"DISABLED"}')).toBe("DISABLED");
    expect(modeFor("not json")).toBe("DISABLED");

    const disabledGate = blockBetween(workflow, "- skipIfDisabled:", "- rejectUnavailableTracker:");
    const disabledCondition = expressionIn(disabledGate, "if:");

    expect(evaluateJq(disabledCondition, { resolution_mode: "APPLY" })).toBe(false);
    expect(evaluateJq(disabledCondition, { resolution_mode: "APPLY" }, false)).toBe(true);
    expect(evaluateJq(disabledCondition, { resolution_mode: "DISABLED" })).toBe(true);
    expect(evaluateJq(disabledCondition, { resolution_mode: "UNKNOWN" })).toBe(true);
    expect(disabledGate).toContain("then: exit");
    expect(workflow.indexOf("- skipIfDisabled:")).toBeLessThan(
      workflow.indexOf("- fetchSavedSummary:"),
    );
    expect(workflow.indexOf("- skipIfDisabled:")).toBeLessThan(
      workflow.indexOf("workerName: lookback-thread-issue-resolution"),
    );
  });

  it("queries one exact saved thread summary and rejects missing, ambiguous, or invalid snapshots", () => {
    expect(jqAvailable, "jq is required to execute workflow expression fixtures").toBe(true);
    const lookup = blockBetween(workflow, "- fetchSavedSummary:", "- selectSavedSummary:");
    const request = JSON.parse(
      evaluateJq(expressionIn(lookup, "json_body:"), {
        workflow: { input: { thread_id: "thread-1" } },
      }) as string,
    ) as Record<string, unknown>;
    expect(lookup).toContain("/llm_memory.service.MemoryService/FindListByCondition");
    expect(request).toEqual({
      external_id: "summary:thread-1",
      memory_kinds: ["MEMORY_KIND_THREAD_SUMMARY"],
      content_types: ["TEXT"],
    });

    const validation = blockBetween(workflow, "- requireSavedSummary:", "- digestSavedSummary:");
    const invalidCondition = expressionIn(validation, "if:");
    const fixture = validSummaryFixture();

    const select = blockBetween(workflow, "- selectSavedSummary:", "- selectSavedSummaryData:");
    const savedSummary = evaluateJq(expressionIn(select, "saved_summary:"), {
      summary_entries: [fixture.summary],
    });
    expect(savedSummary).toEqual(fixture.summary);

    const selectData = blockBetween(
      workflow,
      "- selectSavedSummaryData:",
      "- readSavedSummaryMetadata:",
    );
    const summaryData = evaluateJq(expressionIn(selectData, "summary_data:"), {
      saved_summary: fixture.summary,
    });
    const readMetadata = blockBetween(
      workflow,
      "- readSavedSummaryMetadata:",
      "- selectSummaryCheckpoint:",
    );
    const summaryMetadata = evaluateJq(expressionIn(readMetadata, "summary_metadata:"), {
      summary_data: summaryData,
    });
    expect(summaryMetadata).toEqual(fixture.variables.summary_metadata);

    const summaryCheckpoint = evaluateJq(
      expressionIn(
        blockBetween(workflow, "- selectSummaryCheckpoint:", "- extractSavedSummarySnapshot:"),
        "summary_checkpoint:",
      ),
      { summary_metadata: summaryMetadata },
    );
    const extractSnapshot = blockBetween(
      workflow,
      "- extractSavedSummarySnapshot:",
      "- requireSavedSummary:",
    );
    expect(
      evaluateJq(expressionIn(extractSnapshot, "summary_checkpoint_memory_id:"), {
        summary_checkpoint: summaryCheckpoint,
      }),
    ).toBe("message-9");
    expect(
      evaluateJq(expressionIn(extractSnapshot, "summary_checkpoint_timestamp:"), {
        summary_checkpoint: summaryCheckpoint,
      }),
    ).toBe(1_700_000_000_000);
    expect(evaluateJq(invalidCondition, fixture.variables)).toBe(false);

    for (const changes of [
      { summary_entries: [] },
      { summary_entries: [fixture.summary, fixture.summary] },
      { summary_external_id: "summary:other" },
      { summary_kind: "MEMORY_KIND_RAW" },
      { summary_content_type: "IMAGE" },
      { summary_content: "" },
      { summary_metadata: { ...fixture.variables.summary_metadata, source_thread_id: "other" } },
      { summary_checkpoint_memory_id: "" },
      { summary_checkpoint_timestamp: null },
      { summary_content: null },
    ]) {
      expect(evaluateJq(invalidCondition, { ...fixture.variables, ...changes })).toBe(true);
    }
    expect(validation).toContain("then: exit");
  });

  it("dispatches the child once with the stored summary digest and complete endpoint and policy inputs", () => {
    expect(jqAvailable, "jq is required to execute workflow expression fixtures").toBe(true);
    const digest = blockBetween(workflow, "- digestSavedSummary:", "- dispatchResolution:");
    expect(digest).toContain("runnerName: COMMAND");
    expect(digest).toContain("command -v sha256sum");
    expect(digest).toContain("command -v shasum");
    expect(digest).toContain("stdin: '$" + "{ $summary_content }'");
    const hashArgsLine = digest.match(/args: (\[[^\n]+\])/);
    expect(hashArgsLine).not.toBeNull();
    const [shellFlag, shellScript] = JSON.parse(hashArgsLine?.[1] ?? "[]") as [string, string];
    const checksum = spawnSync("/bin/sh", [shellFlag, shellScript], {
      encoding: "utf8",
      input: "saved summary body",
    });
    expect(checksum.status).toBe(0);
    expect(checksum.stdout.split(" ")[0]).toBe(
      createHash("sha256").update("saved summary body").digest("hex"),
    );
    expect(workflow).toContain(
      "summary_content_digest: '$" + '{ "sha256:" + (.stdout | split(" ")[0]) }\'',
    );

    const dispatch = blockBetween(workflow, "- invokeResolution:", "- classifyChildOutcome:");
    const childInput = JSON.parse(
      evaluateJq(expressionIn(dispatch, "input: >-"), {
        summary_memory_id: "summary-memory-1",
        summary_content_digest: "sha256:abc123",
        summary_checkpoint_memory_id: "message-9",
        summary_checkpoint_at: "1700000000000",
        workflow: {
          input: {
            thread_id: "thread-1",
            memories_grpc_host: "memories.internal",
            memories_grpc_port: 9100,
            memories_grpc_tls: true,
            issue_tracker_grpc_host: "tracker.internal",
            issue_tracker_grpc_port: 9200,
            issue_tracker_grpc_tls: false,
            policy_json: '{"resolution_mode":"APPLY"}',
            llm_worker_name: "memories-llm-test",
            output_language: "en",
          },
        },
      }) as string,
    ) as Record<string, unknown>;

    expect(dispatch).toContain("workerName: lookback-thread-issue-resolution");
    expect(childInput).toEqual({
      thread_id: "thread-1",
      summary_memory_id: "summary-memory-1",
      summary_content_digest: "sha256:abc123",
      summary_checkpoint_memory_id: "message-9",
      summary_checkpoint_at: "1700000000000",
      memories_grpc_host: "memories.internal",
      memories_grpc_port: 9100,
      memories_grpc_tls: true,
      issue_tracker_grpc_host: "tracker.internal",
      issue_tracker_grpc_port: 9200,
      issue_tracker_grpc_tls: false,
      policy_json: '{"resolution_mode":"APPLY"}',
      llm_worker_name: "memories-llm-test",
      output_language: "en",
    });
  });

  it("writes only summary status metadata and classifies child results safely", () => {
    expect(jqAvailable, "jq is required to execute workflow expression fixtures").toBe(true);
    expect(workflow.match(/MemoryService\/Update/g)).toHaveLength(1);
    expect(workflow).not.toMatch(/MemoryService\/(Add|Delete)/);
    expect(workflow).not.toContain("ManagerConsumerService/ResolveCandidate");
    expect(workflow.match(/workerName: lookback-thread-issue-resolution/g)).toHaveLength(1);
    expect(workflow).toContain("resolution_child_output: '$" + "{ .output | fromjson }'");
    expect(workflow).toContain("child_outcome_missing");

    const classify = blockBetween(
      workflow,
      "- classifyChildOutcome:",
      "- recordChildInvocationFailure:",
    );
    const statusExpression = expressionIn(classify, "resolution_status:");
    const statusFor = (child: Record<string, unknown> | null) =>
      evaluateJq(statusExpression, { resolution_child_output: child });

    expect(statusFor({ complete: true, failed_count: 0, unconfirmed_count: 0 })).toBe("COMPLETE");
    expect(statusFor({ complete: false, failed_count: 0, unconfirmed_count: 1 })).toBe(
      "UNCONFIRMED",
    );
    expect(statusFor({ complete: true, failed_count: 0, unconfirmed_count: 1 })).toBe(
      "UNCONFIRMED",
    );
    expect(statusFor({ complete: false, failed_count: 1, unconfirmed_count: 0 })).toBe("FAILED");
    expect(statusFor({ complete: true, failed_count: 1, unconfirmed_count: 0 })).toBe("FAILED");
    expect(statusFor({ complete: false, failed_count: 0, unconfirmed_count: 0 })).toBe("FAILED");
    expect(statusFor(null)).toBe("FAILED");

    const output = workflow.slice(workflow.indexOf("\noutput:"));
    expect(output).toContain("complete: '$" + '{ $resolution_status == "COMPLETE" }\'');
    expect(output).toContain("unconfirmed: '$" + '{ $resolution_status == "UNCONFIRMED" }\'');
    expect(output).toContain("failed: '$" + '{ $resolution_status == "FAILED" }\'');
  });

  it("updates status only after re-reading and matching the saved summary snapshot", () => {
    expect(jqAvailable, "jq is required to execute workflow expression fixtures").toBe(true);
    const childDispatch = workflow.indexOf("- dispatchResolution:");
    const statusUpdate = workflow.indexOf("- persistManualResolutionStatus:");
    expect(childDispatch).toBeGreaterThan(-1);
    expect(statusUpdate).toBeGreaterThan(childDispatch);

    const updateBlock = blockBetween(
      workflow,
      "- persistManualResolutionStatus:",
      "\noutput:",
    );
    const statusGate = expressionIn(updateBlock, "if:");
    expect(
      evaluateJq(statusGate, { resolution_mode: "APPLY", resolution_attempted: true }),
    ).toBe(true);
    expect(
      evaluateJq(statusGate, { resolution_mode: "DISABLED", resolution_attempted: true }),
    ).toBe(false);
    expect(
      evaluateJq(statusGate, { resolution_mode: "APPLY", resolution_attempted: false }),
    ).toBe(false);
    expect(updateBlock).toContain("/llm_memory.service.MemoryService/Find");
    expect(updateBlock).toContain("/llm_memory.service.MemoryService/Update");
    expect(updateBlock).toContain("summary_memory_id");
    expect(updateBlock.indexOf("MemoryService/Find")).toBeLessThan(
      updateBlock.indexOf("MemoryService/Update"),
    );
    const freshDigest = blockBetween(
      updateBlock,
      "- digestCurrentSummaryContent:",
      "- extractCurrentSummarySnapshot:",
    );
    expect(freshDigest).toContain("runnerName: COMMAND");
    expect(freshDigest).toContain("stdin: '$" + "{ $status_summary.data.content // \"\" }'");
    expect(freshDigest).toContain("command -v sha256sum");
    expect(freshDigest).toContain("command -v shasum");

    const snapshotCheck = blockBetween(
      updateBlock,
      "- verifyStatusSummarySnapshot:",
      "- prepareSummaryResolutionStatus:",
    );
    const matchesSnapshot = expressionIn(snapshotCheck, "status_summary_matches_snapshot:");
    const snapshot = {
      status_summary: {
        id: { value: "summary-memory-1" },
        data: { externalId: "summary:thread-1", content: "saved summary body" },
      },
      status_summary_metadata: {
        source_thread_id: "thread-1",
        resolution_high_water_message_id: "message-9",
        resolution_high_water_message_at: 1_700_000_000_000,
      },
      status_summary_memory_id: "summary-memory-1",
      status_summary_external_id: "summary:thread-1",
      status_summary_source_thread_id: "thread-1",
      status_summary_content_digest: "sha256:body-digest",
      summary_content_digest: "sha256:body-digest",
      status_summary_checkpoint_memory_id: "message-9",
      summary_checkpoint_memory_id: "message-9",
      status_summary_checkpoint_at: "1700000000000",
      summary_checkpoint_at: "1700000000000",
      summary_memory_id: "summary-memory-1",
      workflow: { input: { thread_id: "thread-1" } },
    };
    expect(evaluateJq(matchesSnapshot, snapshot)).toBe(true);
    for (const changedSnapshot of [
      { status_summary: null },
      { status_summary_content_digest: "sha256:changed" },
      { status_summary_checkpoint_memory_id: "message-10" },
      { status_summary_checkpoint_at: "1700000000001" },
      {
        status_summary_metadata: {
          ...snapshot.status_summary_metadata,
          resolution_high_water_message_at: "invalid-time",
        },
        status_summary_checkpoint_at: "invalid-time",
      },
      { status_summary_source_thread_id: "other" },
      { status_summary_memory_id: "other-summary" },
    ]) {
      expect(evaluateJq(matchesSnapshot, { ...snapshot, ...changedSnapshot })).toBe(false);
    }

    const writeStep = blockBetween(
      updateBlock,
      "- persistStatusMetadataOnly:",
      "catch:",
    );
    const writeCondition = expressionIn(writeStep, "if:");
    expect(evaluateJq(writeCondition, { status_summary_matches_snapshot: true })).toBe(true);
    expect(evaluateJq(writeCondition, { status_summary_matches_snapshot: false })).toBe(false);
    expect(writeStep).toContain(".metadata = $summary_status_metadata");
    expect(writeStep).not.toContain(".content =");
    expect(updateBlock).toMatch(/MemoryService\/Update[\s\S]*?status_summary_matches_snapshot/);
    expect(updateBlock.indexOf("status_summary_matches_snapshot")).toBeLessThan(
      updateBlock.indexOf("MemoryService/Update"),
    );
    const updateCatch = updateBlock.slice(updateBlock.indexOf("catch:"));
    expect(updateCatch).toContain("recordSummaryStatusUpdateFailure:");
    expect(updateCatch).not.toContain("MemoryService/Update");
    expect(workflow.indexOf("- skipIfDisabled:")).toBeLessThan(statusUpdate);
  });

  it("marks the re-read summary COMPLETE only for committed child completion, otherwise UNKNOWN", () => {
    expect(jqAvailable, "jq is required to execute workflow expression fixtures").toBe(true);
    const updateBlock = blockBetween(
      workflow,
      "- persistManualResolutionStatus:",
      "\noutput:",
    );
    const statusSelection = blockBetween(
      updateBlock,
      "- prepareSummaryResolutionStatus:",
      "- prepareSummaryStatusMetadata:",
    );
    const targetStatus = expressionIn(statusSelection, "requested_summary_status:");
    expect(evaluateJq(targetStatus, { resolution_status: "COMPLETE" })).toBe("COMPLETE");
    expect(evaluateJq(targetStatus, { resolution_status: "UNCONFIRMED" })).toBe("UNKNOWN");
    expect(evaluateJq(targetStatus, { resolution_status: "FAILED" })).toBe("UNKNOWN");

    const metadataStep = blockBetween(
      updateBlock,
      "- prepareSummaryStatusMetadata:",
      "- persistStatusMetadataOnly:",
    );
    const metadataExpression = expressionIn(metadataStep, "summary_status_metadata:");
    const savedMetadata = {
      source_thread_id: "thread-1",
      resolution_high_water_message_id: "message-9",
      resolution_high_water_message_at: 1_700_000_000_000,
      issue_resolution_status: "PENDING",
      other_field: "preserved",
    };
    const serializedMetadata = evaluateJq(metadataExpression, {
      status_summary_metadata: savedMetadata,
      requested_summary_status: "COMPLETE",
    });
    expect(typeof serializedMetadata).toBe("string");
    expect(JSON.parse(serializedMetadata as string)).toEqual({
      ...savedMetadata,
      issue_resolution_status: "COMPLETE",
    });

    const writeStep = blockBetween(
      updateBlock,
      "- persistStatusMetadataOnly:",
      "catch:",
    );
    const writePayload = JSON.parse(
      evaluateJq(expressionIn(writeStep, "json_body: >-"), {
        status_summary: {
          id: { value: "summary-memory-1" },
          data: { content: "saved summary body", externalId: "summary:thread-1" },
        },
        summary_status_metadata: serializedMetadata,
      }) as string,
    ) as { id: unknown; data: Record<string, unknown> };
    expect(writePayload.id).toEqual({ value: "summary-memory-1" });
    expect(writePayload.data.content).toBe("saved summary body");
    expect(JSON.parse(writePayload.data.metadata as string)).toEqual({
      ...savedMetadata,
      issue_resolution_status: "COMPLETE",
    });
  });
});
