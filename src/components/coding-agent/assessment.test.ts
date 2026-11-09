import { describe, expect, it } from "vitest";
import type { CodingAgentApprovalAssessmentContract } from "@/types/api";
import {
  approvalContractMatchesCanonicalCriteria,
  decodeAgentReport,
  isCodingAgentApprovalAssessmentContract,
} from "./assessment";

const approvalContract = {
  schema_version: 1,
  purpose: "implementation",
  criteria: [
    {
      criterion_id: "AC-1",
      statement: "One criterion\nwith a second line",
      required: true,
      method: "unknown",
    },
  ],
  validation_plan: [
    {
      validation_id: "V-1",
      required: true,
      command: null,
    },
  ],
  provider: null,
  prompt_policy_version: null,
  input_scope: {
    allowed_fields: ["issue_title", "issue_body", "validation_result"],
    allowed_sources: [
      { kind: "issue_context", identity: "issue:issue-1", max_bytes: 32768 },
      { kind: "validation_result", identity: "V-1", max_bytes: 32768 },
    ],
    max_input_bytes: 131072,
  },
  judge_input_scope_digest: "b".repeat(64),
} satisfies CodingAgentApprovalAssessmentContract;

function reportBytes(overrides: Record<string, unknown> = {}) {
  return new TextEncoder().encode(
    JSON.stringify({
      schema_version: 1,
      run_id: "run-1",
      attempt_id: "attempt-1",
      task_kind: "implementation",
      status: "blocked",
      summary: "The agent reports a blocker.",
      acceptance: [{ criterion_id: "AC-1", assessment: "unknown", evidence: [] }],
      validation: [
        {
          validation_id: "V-1",
          claimed_result: "not_run",
          evidence: [],
          reason: "No command was approved.",
        },
      ],
      investigation: null,
      implementation: {
        change_summary: "No changes were made.",
        changed_files: [],
        implementation_notes: [],
      },
      unresolved: [
        {
          description: "The required check is unavailable.",
          blocks_completion: true,
          criterion_ids: ["AC-1"],
        },
      ],
      ...overrides,
    }),
  );
}

describe("coding-agent approval assessment contract", () => {
  it("preserves the backend's ordered canonical criteria and refuses missing or duplicate IDs", () => {
    expect(isCodingAgentApprovalAssessmentContract(approvalContract, "a".repeat(64), false)).toBe(
      true,
    );
    expect(
      approvalContractMatchesCanonicalCriteria(approvalContract, [
        "One criterion\nwith a second line",
      ]),
    ).toBe(true);
    expect(
      approvalContractMatchesCanonicalCriteria(approvalContract, [
        "One criterion",
        "with a second line",
      ]),
    ).toBe(false);

    expect(
      isCodingAgentApprovalAssessmentContract(
        {
          ...approvalContract,
          criteria: [approvalContract.criteria[0], approvalContract.criteria[0]],
        },
        "a".repeat(64),
        false,
      ),
    ).toBe(false);
    expect(
      isCodingAgentApprovalAssessmentContract(
        { ...approvalContract, criteria: [] },
        "a".repeat(64),
        false,
      ),
    ).toBe(true);
  });

  it("renders agent-reported blocked status as report data, not a trusted completion result", () => {
    const report = decodeAgentReport(reportBytes());
    expect(report?.status).toBe("blocked");
    expect(report?.acceptance[0]?.assessment).toBe("unknown");
    expect(report?.validation[0]?.claimed_result).toBe("not_run");
    expect(report?.unresolved[0]?.blocks_completion).toBe(true);
  });

  it("rejects unsupported, malformed, and over-bounded reports", () => {
    expect(decodeAgentReport(reportBytes({ schema_version: 2 }))).toBeNull();
    expect(decodeAgentReport(new TextEncoder().encode("not JSON"))).toBeNull();
    expect(decodeAgentReport(new Uint8Array(1024 * 1024 + 1))).toBeNull();
  });

  it("rejects duplicate JSON keys including escaped-equivalent and nested names", () => {
    const validJson = new TextDecoder().decode(reportBytes());
    const duplicateStatus = validJson.replace(
      '"status":"blocked",',
      '"status":"blocked","status":"completed",',
    );
    const escapedDuplicateStatus = validJson.replace(
      '"status":"blocked",',
      '"status":"blocked","\\u0073tatus":"completed",',
    );
    const duplicateNestedKey = validJson.replace(
      '"change_summary":"No changes were made.",',
      '"change_summary":"No changes were made.","change_summary":"Completed",',
    );

    expect(decodeAgentReport(new TextEncoder().encode(duplicateStatus))).toBeNull();
    expect(decodeAgentReport(new TextEncoder().encode(escapedDuplicateStatus))).toBeNull();
    expect(decodeAgentReport(new TextEncoder().encode(duplicateNestedKey))).toBeNull();
  });

  it("rejects JSON nested beyond the bounded scanner depth", () => {
    const validJson = new TextDecoder().decode(reportBytes());
    const tooDeep = validJson.replace(
      ',"unresolved":',
      `,"nested":${"[".repeat(65)}0${"]".repeat(65)},"unresolved":`,
    );

    expect(decodeAgentReport(new TextEncoder().encode(tooDeep))).toBeNull();
  });

  it.each([
    ["both task-specific sections are null", { investigation: null, implementation: null }],
    [
      "both task-specific sections are present",
      {
        investigation: { scope: [], findings: [], limitations: [], response_questions: [] },
      },
    ],
    [
      "an implementation report has only investigation details",
      {
        investigation: { scope: [], findings: [], limitations: [], response_questions: [] },
        implementation: null,
      },
    ],
    ["an investigation report has only implementation details", { task_kind: "investigation" }],
  ])("rejects reports whose task kind disagrees with sections (%s)", (_name, overrides) => {
    expect(decodeAgentReport(reportBytes(overrides))).toBeNull();
  });

  it("accepts an investigation report with exactly its matching section", () => {
    expect(
      decodeAgentReport(
        reportBytes({
          task_kind: "investigation",
          investigation: { scope: [], findings: [], limitations: [], response_questions: [] },
          implementation: null,
        }),
      ),
    ).not.toBeNull();
  });
});

it("accepts literal empty arguments and multiline validation scripts but rejects NUL and oversized command bytes", () => {
  const contract: CodingAgentApprovalAssessmentContract = structuredClone(approvalContract);
  const definition = contract.validation_plan.at(0);
  if (!definition) throw new Error("fixture validation is missing");
  definition.command = {
    argv: ["python", "-c", "print(1)\nprint(2)", ""],
    working_directory: "/workspace/crate",
    permissions: ["read_workspace", "write_build_output"],
    profile_digest: "a".repeat(64),
    timeout_ms: 43000,
  };
  expect(isCodingAgentApprovalAssessmentContract(contract, "a".repeat(64), false)).toBe(true);
  definition.command.argv[2] = "bad\u0000";
  expect(isCodingAgentApprovalAssessmentContract(contract, "a".repeat(64), false)).toBe(false);
  definition.command.argv = ["python", ...Array.from({ length: 5 }, () => "a".repeat(16000))];
  expect(isCodingAgentApprovalAssessmentContract(contract, "a".repeat(64), false)).toBe(false);
});

it("matches explicit empty canonical criteria without treating missing criteria as empty", () => {
  const empty = { ...approvalContract, criteria: [] };
  expect(approvalContractMatchesCanonicalCriteria(empty, [])).toBe(true);
  expect(approvalContractMatchesCanonicalCriteria(empty, undefined)).toBe(false);
  expect(approvalContractMatchesCanonicalCriteria(empty, ["A condition"])).toBe(false);
  expect(approvalContractMatchesCanonicalCriteria(approvalContract, [])).toBe(false);
});
