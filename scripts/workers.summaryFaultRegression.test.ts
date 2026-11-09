import { describe, expect, it } from "vitest";
import extract from "../workers/workflows/issue-extract/issue-extract.yaml?raw";
import group from "../workers/workflows/thread-group-summary/thread-group-summary-batch.yaml?raw";
import single from "../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml?raw";
import { blockBetween, evaluate } from "./lib/workflowJq";

function initializedOutput(source: string, initStep: string, outputField: string): unknown {
  const init = blockBetween(source, initStep, "\n  - ");
  const output = source.slice(source.lastIndexOf("\noutput:"));
  const initLine = init.split("\n").find((line) => line.trimStart().startsWith(`${outputField}:`));
  const outputLine = output.split("\n").find((line) => line.trimStart().startsWith(`${outputField}:`));
  expect(outputLine).toBeDefined();
  const expression = outputLine?.match(/\$\{\s*(.*?)\s*\}/)?.[1];
  expect(expression).toBeDefined();
  const variables = initLine == null ? {} : { [outputField]: null };
  return evaluate(expression as string, variables);
}

describe("issue extraction and summary final output on skipped or faulted children", () => {
  it("keeps extraction early exits from faulting on the unvisited candidate shaping step", () => {
    const skips = blockBetween(extract, "skipIfDisabled:", "extractAfterSignal:");
    expect(skips).toContain("skipIfNoSignal:");
    expect(skips).toContain("then: exit");
    expect(initializedOutput(extract, "- init:", "shaped")).toBeNull();
  });

  it("keeps a saved summary successful when extraction is disabled or skipped", () => {
    const extraction = blockBetween(single, "- extractIssues:", "\noutput:");
    expect(extraction).toContain("extraction_enabled");
    expect(initializedOutput(single, "- initOutputVars:", "extract_error")).toBeNull();
  });

  it("reports a faulted child output rather than treating it as successful extraction", () => {
    const extraction = blockBetween(single, "- extractIssues:", "\noutput:");
    expect(extraction).toContain("rejectFaultedExtractResult:");
    const guard = blockBetween(extraction, "rejectFaultedExtractResult:", "captureExtractOutcome:");
    const condition = guard.match(/if:\s*(?:>-\s*)?['"]?\$\{\s*(.*?)\s*\}['"]?/s)?.[1];
    expect(condition).toBeDefined();
    expect(evaluate(condition as string, { extract_child_result: { error: "faulted" } })).toBe(true);
    expect(evaluate(condition as string, { extract_child_result: null })).toBe(true);
    expect(
      evaluate(condition as string, {
        extract_child_result: { skipped: false, result: "failed: tracker not ready" },
      }),
    ).toBe(true);
    expect(evaluate(condition as string, { extract_child_result: { skipped: true } })).toBe(false);
    expect(
      evaluate(condition as string, {
        extract_child_result: { skipped: false, extract: { applied: { runState: "COMMITTED" } } },
      }),
    ).toBe(false);
  });

  it("preserves group member-refresh success without an extraction policy", () => {
    const refresh = blockBetween(group, "- refreshMemberSummaries:", "- summarizeEachGroup:");
    expect(refresh).not.toContain("policy_json:");
    expect(initializedOutput(single, "- initOutputVars:", "extract_error")).toBeNull();
  });
});
