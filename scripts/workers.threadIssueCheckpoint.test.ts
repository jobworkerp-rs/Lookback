import { spawnSync } from "node:child_process";
import { describe, expect, it } from "vitest";
import workflow from "../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml?raw";

const jqAvailable = spawnSync("jq", ["--version"], { encoding: "utf8" }).status === 0;

function blockBetween(source: string, startMarker: string, endMarker: string): string {
  const start = source.indexOf(startMarker);
  expect(start, `missing ${startMarker}`).toBeGreaterThan(-1);
  const end = source.indexOf(endMarker, start);
  expect(end, `missing ${endMarker}`).toBeGreaterThan(start);
  return source.slice(start, end);
}

function expressionUntil(source: string, field: string, nextMarker: string): string {
  const fieldMarker = source.indexOf(`\n        ${field}: >-`);
  expect(fieldMarker, `missing ${field}`).toBeGreaterThan(-1);
  const fieldIndex = fieldMarker + 1;
  const expressionStart = source.indexOf("${", fieldIndex);
  expect(expressionStart, `missing expression for ${field}`).toBeGreaterThan(fieldIndex);
  const nextIndex = source.indexOf(nextMarker, expressionStart);
  expect(nextIndex, `missing end marker ${nextMarker}`).toBeGreaterThan(expressionStart);
  const fieldBody = source.slice(expressionStart + 2, nextIndex);
  const expressionEnd = fieldBody.lastIndexOf("}");
  expect(expressionEnd, `missing expression end for ${field}`).toBeGreaterThan(0);
  return fieldBody.slice(0, expressionEnd).trim();
}

function evaluateJq(expression: string, variables: Record<string, unknown>): unknown {
  const args = ["-c", "-n"];
  for (const [name, value] of Object.entries(variables)) {
    args.push("--argjson", name, JSON.stringify(value));
  }
  args.push(expression);

  const result = spawnSync("jq", args, {
    encoding: "utf8",
    env: { ...process.env, LOOKBACK_EXPERIMENTAL_ISSUES_ENABLED: "true" },
  });
  expect(result.error).toBeUndefined();
  expect(result.status, `${result.stderr}\n${expression}`).toBe(0);
  return JSON.parse(result.stdout.trim());
}

describe("single-thread summary resolution checkpoint", () => {
  it("requires jq rather than silently skipping expression fixtures", () => {
    expect(jqAvailable, "jq is required to execute workflow expression fixtures").toBe(true);
  });

  it("derives the processed checkpoint from the last ordered raw text message", () => {
    const currentCheckpoint = blockBetween(
      workflow,
      "- deriveCurrentMessageCheckpoint:",
      "- checkMessageCount:",
    );

    expect(workflow).toContain('roles: ["ROLE_USER", "ROLE_ASSISTANT"]');
    expect(workflow).toContain('content_types: ["TEXT"]');
    expect(currentCheckpoint).toContain("$messages[-1]");
    expect(currentCheckpoint).toContain(".id.value");
    expect(currentCheckpoint).toContain(".data.createdAt");
    expect(currentCheckpoint).toContain("current_message_checkpoint_valid");
    expect(currentCheckpoint).toContain("tonumber) catch null");
    expect(currentCheckpoint).toContain("current_message_at != null");
  });

  it("uses a legacy summary only to establish a baseline, not an eligible watermark", () => {
    const prepareCheckpoint = blockBetween(
      workflow,
      "- prepareResolutionCheckpoint:",
      "- cleanupStaleSummaries:",
    );

    expect(prepareCheckpoint).toContain("is_legacy_message_checkpoint");
    expect(prepareCheckpoint).toContain("last_processed_message_id");
    expect(prepareCheckpoint).toContain("last_processed_message_at");
    expect(prepareCheckpoint).toContain("existing_summary_memory == null");
    expect(prepareCheckpoint).toContain("resolution_high_water_message_id");
    expect(prepareCheckpoint).toContain("resolution_high_water_message_at");
    expect(prepareCheckpoint).toContain("current_message_checkpoint_valid");
  });

  it("advances eligibility only for a strictly later message under APPLY and without force", () => {
    const eligibility = blockBetween(
      workflow,
      "resolution_high_water_should_advance:",
      "- prepareSummaryMetadata:",
    );

    expect(eligibility).toContain("previous_message_checkpoint_valid");
    expect(eligibility).toContain("current_message_position > $previous_message_position");
    expect(eligibility).toContain("current_message_at > $previous_message_at");
    expect(eligibility).toContain('resolution_mode == "APPLY"');
    expect(eligibility).toContain('env.LOOKBACK_EXPERIMENTAL_ISSUES_ENABLED == "true"');
    expect(eligibility).toContain("force_resummarize | not");
  });

  it("preserves the previous high-water mark unless a newer eligible mark replaces it", () => {
    const metadata = blockBetween(
      workflow,
      "resolution_high_water_should_advance:",
      "- updateSummary:",
    );

    expect(metadata).toContain("previous_resolution_high_water_at");
    expect(metadata).toContain("current_message_at > $previous_resolution_high_water_at");
    expect(metadata).toContain("$existing_summary_metadata.resolution_high_water_message_id");
    expect(metadata).toContain("$existing_summary_metadata.resolution_high_water_message_at");
  });

  it("preserves existing metadata and writes the checkpoint with the summary in one operation", () => {
    const update = blockBetween(workflow, "- updateSummary:", "- extractIssues:");
    const add = blockBetween(workflow, "- addSummaryMemory:", "- extractIssues:");
    const preparedMetadata = blockBetween(
      workflow,
      "- prepareSummaryMetadata:",
      "- cleanupStaleSummaries:",
    );

    expect(workflow).toContain('$existing_summary_memory.data.metadata // "{}"');
    expect(preparedMetadata).toMatch(/\$existing_summary_metadata\s*\+/);
    expect(update).toContain(".metadata = $summary_metadata");
    expect(add).toContain("metadata: $summary_metadata");
    expect(workflow.match(/MemoryService\/Update/g)).toHaveLength(2);
    expect(workflow.indexOf("- runThreadIssueResolution:")).toBeGreaterThan(
      workflow.indexOf("- addSummaryMemory:"),
    );
  });

  it("validates ordered checkpoints across multiple messages and rejects decreasing timestamps", () => {
    const checkpointExpression = expressionUntil(
      workflow,
      "current_message_checkpoint_valid",
      "- checkMessageCount:",
    );
    const message = (id: string, createdAt: number) => ({
      id: { value: id },
      data: {
        role: "ROLE_USER",
        contentType: "TEXT",
        createdAt,
      },
    });

    const orderedMessages = [
      message("message-1", 100),
      message("message-2", 200),
      message("message-3", 300),
    ];
    expect(
      evaluateJq(checkpointExpression, {
        messages: orderedMessages,
        current_message_id: "message-3",
        current_message_at: 300,
      }),
    ).toBe(true);

    const outOfOrderMessages = [
      message("message-1", 100),
      message("message-2", 300),
      message("message-3", 200),
    ];
    expect(
      evaluateJq(checkpointExpression, {
        messages: outOfOrderMessages,
        current_message_id: "message-3",
        current_message_at: 200,
      }),
    ).toBe(false);
  });

  it("advances from a baseline save with null high-water fields on the next APPLY save", () => {
    const absentExpression = expressionUntil(
      workflow,
      "previous_resolution_high_water_absent",
      "resolution_mode:",
    );
    const advancementExpression = expressionUntil(
      workflow,
      "resolution_high_water_should_advance",
      "- prepareSummaryMetadata:",
    );
    const metadataExpression = expressionUntil(
      workflow,
      "summary_metadata",
      "resolution_status: >-",
    );

    const firstSavedJson = evaluateJq(metadataExpression, {
      existing_summary_metadata: {},
      workflow: { input: { thread_id: "thread-1" } },
      summary_json: { category: "general" },
      processed_message_checkpoint_should_advance: true,
      current_message_id: "message-1",
      current_message_at: 100,
      resolution_high_water_should_advance: false,
    });
    expect(typeof firstSavedJson).toBe("string");
    const firstSaved = JSON.parse(firstSavedJson as string) as Record<string, unknown>;
    expect(firstSaved).toMatchObject({
      last_processed_message_id: "message-1",
      last_processed_message_at: 100,
      resolution_high_water_message_id: null,
      resolution_high_water_message_at: null,
    });

    const highWaterAbsent = evaluateJq(absentExpression, {
      existing_summary_memory: { id: "summary-memory" },
      existing_summary_metadata: firstSaved,
    });
    expect(highWaterAbsent).toBe(true);

    const shouldAdvance = evaluateJq(advancementExpression, {
      existing_summary_memory: { id: "summary-memory" },
      previous_message_checkpoint_valid: true,
      current_message_checkpoint_valid: true,
      current_message_position: 1,
      previous_message_position: 0,
      current_message_at: 200,
      previous_message_at: 100,
      resolution_mode: "APPLY",
      workflow: { input: { force_resummarize: false } },
      previous_resolution_high_water_absent: highWaterAbsent,
      previous_resolution_high_water_id: "",
      previous_resolution_high_water_at: null,
    });
    expect(shouldAdvance).toBe(true);

    const secondSavedJson = evaluateJq(metadataExpression, {
      existing_summary_metadata: firstSaved,
      workflow: { input: { thread_id: "thread-1" } },
      summary_json: { category: "general" },
      processed_message_checkpoint_should_advance: true,
      current_message_id: "message-2",
      current_message_at: 200,
      resolution_high_water_should_advance: shouldAdvance,
    });
    expect(typeof secondSavedJson).toBe("string");
    const secondSaved = JSON.parse(secondSavedJson as string) as Record<string, unknown>;
    expect(secondSaved).toMatchObject({
      last_processed_message_id: "message-2",
      last_processed_message_at: 200,
      resolution_high_water_message_id: "message-2",
      resolution_high_water_message_at: 200,
    });
  });

  it("fails closed for a partially missing or partially null high-water pair", () => {
    const absentExpression = expressionUntil(
      workflow,
      "previous_resolution_high_water_absent",
      "resolution_mode:",
    );
    expect(
      evaluateJq(absentExpression, {
        existing_summary_memory: { id: "summary-memory" },
        existing_summary_metadata: {},
      }),
    ).toBe(true);
    for (const metadata of [
      { resolution_high_water_message_id: null },
      { resolution_high_water_message_at: null },
      { resolution_high_water_message_id: "message-1", resolution_high_water_message_at: null },
      { resolution_high_water_message_id: null, resolution_high_water_message_at: 100 },
    ]) {
      expect(
        evaluateJq(absentExpression, {
          existing_summary_memory: { id: "summary-memory" },
          existing_summary_metadata: metadata,
        }),
      ).toBe(false);
    }
  });
});
