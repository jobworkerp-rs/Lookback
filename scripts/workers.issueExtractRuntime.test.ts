import { describe, expect, it } from "vitest";
import issueExtractWorkflow from "../workers/workflows/issue-extract/issue-extract.yaml?raw";
import { runStep } from "./lib/workflowJq";

describe("issue extract pre-apply failure classification", () => {
  const reason = (bindings: Record<string, unknown>) =>
    runStep(issueExtractWorkflow, "preparePreApplyFailure", "pre_apply_failure_reason", bindings);

  it("reports a digest mismatch before looking at the shaped output", () => {
    expect(reason({ source_changed: true, shaped: null })).toBe("digest_mismatch");
  });

  it("reports corrupt JSON only when the shaped output says so", () => {
    expect(reason({ source_changed: false, shaped: { candidates: [], fatal: true } })).toBe(
      "json_corrupt",
    );
    expect(reason({ source_changed: false, shaped: { candidates: [], fatal: false } })).toBeNull();
  });

  it("does not fail on a missing or non-object shaped output", () => {
    // Observed in production: `shaped` was an array and `.fatal` raised,
    // replacing the real extraction failure with a jq error.
    expect(reason({ source_changed: false, shaped: [] })).toBeNull();
    expect(reason({ source_changed: false, shaped: null })).toBeNull();
  });
});
