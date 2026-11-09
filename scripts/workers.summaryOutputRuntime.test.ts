import { describe, expect, it } from "vitest";
import daily from "../workers/lang-workers/workers/daily-work-summary/daily-work-summary-single.yaml?raw";
import monthly from "../workers/lang-workers/workers/monthly-work-summary/monthly-work-summary-single.yaml?raw";
import group from "../workers/lang-workers/workers/thread-group-summary/thread-group-summary-single.yaml?raw";
import thread from "../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml?raw";
import weekly from "../workers/lang-workers/workers/weekly-work-summary/weekly-work-summary-single.yaml?raw";
import { runStep } from "./lib/workflowJq";

const llmOutput = (value: unknown) => ({
  content: { text: typeof value === "string" ? value : JSON.stringify(value) },
});

const periodGroups = [
  { purpose: "移行", bullets: ["a"], source_memory_ids: ["1"], status: "resolved" },
];

const periodCases = [
  {
    name: "daily",
    workflow: daily,
    generate: "generateDailySummary",
    validate: "validateDailySummaryShape",
    valid: { overall_purpose: "目的", purpose_groups: periodGroups, by_topic: [], carryover: [] },
  },
  {
    name: "weekly",
    workflow: weekly,
    generate: "generateWeeklySummary",
    validate: "validateWeeklySummaryShape",
    valid: {
      overall_purpose: "目的",
      purpose_groups: periodGroups,
      by_topic: [],
      trends: [],
      carryover: [],
    },
  },
  {
    name: "monthly",
    workflow: monthly,
    generate: "generateMonthlySummary",
    validate: "validateMonthlySummaryShape",
    valid: {
      overall_purpose: "目的",
      purpose_groups: periodGroups,
      by_topic: [],
      highlights: [],
      milestones: [],
      carryover: [],
    },
  },
];

const threadValid = {
  category: "coding",
  title: "題名",
  summary: "## 目的\n本文",
  key_decisions: [],
  status: "resolved",
  has_unfinished_issue_candidates: false,
};

describe.each(periodCases)("$name summary output validation", ({ workflow, generate, validate, valid }) => {
  const parse = (value: unknown) => runStep(workflow, generate, "summary_json", {}, llmOutput(value));
  const rejects = (summary: unknown) =>
    runStep(workflow, validate, "if", { summary_json: summary }) as boolean;

  it("accepts a filled summary", () => {
    expect(rejects(parse(valid))).toBe(false);
  });

  // A thinking model can give up inside constrained JSON decoding and return
  // the schema skeleton with every field blank.
  it("rejects the blank skeleton a constrained thinking model returns", () => {
    const blank = Object.fromEntries(
      Object.entries(valid).map(([key, value]) => [key, typeof value === "string" ? "" : []]),
    );
    expect(rejects(parse(blank))).toBe(true);
  });

  it("rejects whitespace-only purposes and empty purpose groups", () => {
    expect(rejects({ ...valid, overall_purpose: " \n " })).toBe(true);
    expect(rejects({ ...valid, purpose_groups: [] })).toBe(true);
    expect(
      rejects({ ...valid, purpose_groups: [{ ...periodGroups[0], purpose: "" }] }),
    ).toBe(true);
  });

  it("turns malformed JSON into a rejected output instead of an expression error", () => {
    expect(parse('{"overall_purpose": "trunc')).toBeNull();
    expect(rejects(null)).toBe(true);
  });
});

const titledCases = [
  { name: "thread", workflow: thread, validate: "validateThreadSummaryOutput", bindings: {} },
  {
    name: "thread group",
    workflow: group,
    validate: "validateGroupSummaryOutput",
    bindings: { source_member_thread_ids: ["7"] },
  },
];

describe.each(titledCases)("$name summary output validation", ({ workflow, validate, bindings }) => {
  const parse = (value: unknown) =>
    runStep(workflow, "generateSummary", "summary_json", bindings, llmOutput(value));
  const rejects = (summary: unknown) =>
    runStep(workflow, validate, "if", { summary_json: summary }) as boolean;

  it("accepts a titled summary", () => {
    expect(rejects(parse(threadValid))).toBe(false);
  });

  it("rejects a blank title or body", () => {
    expect(rejects(parse({ ...threadValid, title: "", summary: "" }))).toBe(true);
    expect(rejects({ ...threadValid, title: "  " })).toBe(true);
    expect(rejects({ ...threadValid, summary: "\n" })).toBe(true);
  });

  it("turns malformed JSON into a rejected output", () => {
    expect(parse("{")).toBeNull();
    expect(rejects(null)).toBe(true);
  });
});

it("keeps the member IDs on a thread group summary", () => {
  const summary = runStep(
    group,
    "generateSummary",
    "summary_json",
    { source_member_thread_ids: ["7"] },
    llmOutput(threadValid),
  ) as Record<string, unknown>;
  expect(summary.source_thread_ids).toEqual(["7"]);
});
