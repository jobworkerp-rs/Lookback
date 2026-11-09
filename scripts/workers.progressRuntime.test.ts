import { describe, expect, it } from "vitest";
import llmWorkers from "../workers/llm-workers.yaml?raw";
import dailyBatch from "../workers/workflows/daily-work-summary/daily-work-summary-batch.yaml?raw";
import monthlyBatch from "../workers/workflows/monthly-work-summary/monthly-work-summary-batch.yaml?raw";
import issueResolutionBatch from "../workers/workflows/thread-group-issue-resolution/thread-group-issue-resolution-batch.yaml?raw";
import weeklyBatch from "../workers/workflows/weekly-work-summary/weekly-work-summary-batch.yaml?raw";
import { evaluate, outputExpression, runStep, stepExpression } from "./lib/workflowJq";

type ProgressCase = {
  name: string;
  workflow: string;
  loop: string;
  index: string;
  list: string;
  processedCount: string;
  firstWorkStep: string;
  targets: unknown[];
};

const cases: ProgressCase[] = [
  {
    name: "daily periods",
    workflow: dailyBatch,
    loop: "summarizeEachDay",
    index: "date_index",
    list: "date_list",
    processedCount: "processed_dates",
    firstWorkStep: "abortBeforeNextDateAfterPathFailure",
    targets: runStep(dailyBatch, "resolveDateRange", "date_list", {
      yesterday_epoch: 0,
      workflow: { input: { target_dates: ["2026-10-06", "2026-10-05"] } },
    }) as unknown[],
  },
  {
    name: "weekly periods",
    workflow: weeklyBatch,
    loop: "summarizeEachWeek",
    index: "week_index",
    list: "week_list",
    processedCount: "processed_weeks",
    firstWorkStep: "abortBeforeNextWeekAfterPathFailure",
    targets: runStep(weeklyBatch, "resolveWeekRange", "week_list", {
      last_week_monday_epoch: 0,
      workflow: { input: { target_weeks: ["2026-W41", "2026-W40"] } },
    }) as unknown[],
  },
  {
    name: "monthly periods",
    workflow: monthlyBatch,
    loop: "summarizeEachMonth",
    index: "month_index",
    list: "month_list",
    processedCount: "processed_months",
    firstWorkStep: "abortBeforeNextMonthAfterPathFailure",
    targets: runStep(monthlyBatch, "resolveMonthRange", "month_list", {
      last_month_token: "2026-09",
      workflow: { input: { target_months: ["2026-10", "2026-09"] } },
    }) as unknown[],
  },
  {
    name: "ThreadGroup issue candidates",
    workflow: issueResolutionBatch,
    loop: "resolveEachCandidate",
    index: "candidate_index",
    list: "review_rows",
    processedCount: "candidate_count",
    firstWorkStep: "initCandidateState",
    targets: [{ candidate: { candidateId: "candidate-1" } }, { candidate: { candidateId: "candidate-2" } }],
  },
];

function loopBlock(testCase: ProgressCase): string {
  const start = testCase.workflow.indexOf(`- ${testCase.loop}:`);
  expect(start, `${testCase.loop} loop is present`).toBeGreaterThanOrEqual(0);
  const nextTask = testCase.workflow.indexOf("\n   - ", start + 1);
  const output = testCase.workflow.indexOf("\noutput:", start + 1);
  const ends = [nextTask, output].filter((index) => index >= 0);
  const end = ends.length > 0 ? Math.min(...ends) : undefined;
  return testCase.workflow.slice(start, end);
}

function progressAt(testCase: ProgressCase, index: number) {
  const bindings = { [testCase.index]: index, [testCase.list]: testCase.targets };
  return {
    processed: runStep(testCase.workflow, "reportProgress", "progress_processed", bindings),
    total: runStep(testCase.workflow, "reportProgress", "progress_total", bindings),
  };
}

function loopInputExpression(testCase: ProgressCase): string {
  const line = loopBlock(testCase).split("\n").find((candidate) => /^\s+in:\s*/.test(candidate));
  const match = line?.match(/in:\s*"?\$\{(.*)\}"?\s*$/);
  expect(match, `${testCase.loop} for.in expression is present`).toBeDefined();
  return match![1].trim();
}

function progressEventsExpression(testCase: ProgressCase): string {
  const loopInput = loopInputExpression(testCase);
  const processed = stepExpression(testCase.workflow, "reportProgress", "progress_processed")
    .replaceAll(`$${testCase.index}`, "$loop_index")
    .replaceAll(`$${testCase.list}`, "$loop_targets");
  const total = stepExpression(testCase.workflow, "reportProgress", "progress_total").replaceAll(
    `$${testCase.list}`,
    "$loop_targets",
  );
  return `(${loopInput}) as $loop_targets
    | [range(0; ($loop_targets | length)) as $loop_index
       | {progress_processed: (${processed}), progress_total: (${total})}]`;
}

function outputValue(workflow: string, key: string, bindings: Record<string, unknown>): unknown {
  return evaluate(outputExpression(workflow, key), bindings);
}

function initializeFields(
  workflow: string,
  step: string,
  fields: string[],
  initialBindings: Record<string, unknown>,
): Record<string, unknown> {
  const bindings = { ...initialBindings };
  for (const field of fields) {
    bindings[field] = runStep(workflow, step, field, bindings);
  }
  return bindings;
}

// jq's libc-backed strptime ignores ISO week directives; reproduce the ISO
// week ordering needed by this reversed-range test while running the actual YAML expression.
const WEEK_STRPTIME_SHIM = `
def strptime($format):
  if $format == "%G-W%V-%u" then
    capture("^(?<year>[0-9]{4})-W(?<week>[0-9]{2})-(?<weekday>[1-7])$")
    | [(.year | tonumber), 0, (((.week | tonumber) - 1) * 7 + (.weekday | tonumber)), 0, 0, 0, 0, 0]
  else error("unexpected strptime format") end;
`;

function emptyState(testCase: ProgressCase): Record<string, unknown> {
  if (testCase.workflow === dailyBatch) {
    const workflow = {
      input: {
        target_dates: [],
        start_date: "2026-10-06",
        end_date: "2026-10-05",
        last_n_days: 0,
        timezone_offset_hours: 9,
      },
    };
    const date_list = runStep(dailyBatch, "resolveDateRange", "date_list", {
      workflow,
      yesterday_epoch: 0,
    });
    return initializeFields(
      dailyBatch,
      "initOutcomeAccumulators",
      ["generated_dates", "skipped_dates", "failed_dates", "failed_path_buckets", "last_error"],
      { workflow, date_list },
    );
  }

  if (testCase.workflow === weeklyBatch) {
    const workflow = {
      input: {
        target_weeks: [],
        start_week: "2026-W41",
        end_week: "2026-W40",
        last_n_weeks: 0,
        timezone_offset_hours: 9,
      },
    };
    const week_list = evaluate(
      `${WEEK_STRPTIME_SHIM}${stepExpression(weeklyBatch, "resolveWeekRange", "week_list")}`,
      { workflow, last_week_monday_epoch: 0 },
    );
    return initializeFields(
      weeklyBatch,
      "initOutcomeAccumulators",
      ["generated_weeks", "skipped_weeks", "failed_weeks", "failed_path_buckets", "last_error"],
      { workflow, week_list },
    );
  }

  if (testCase.workflow === monthlyBatch) {
    const workflow = {
      input: {
        target_months: [],
        start_month: "2026-10",
        end_month: "2026-09",
        last_n_months: 0,
        timezone_offset_hours: 9,
      },
    };
    const month_list = runStep(monthlyBatch, "resolveMonthRange", "month_list", {
      workflow,
      last_month_token: "2026-09",
    });
    return initializeFields(
      monthlyBatch,
      "initOutcomeAccumulators",
      ["generated_months", "skipped_months", "failed_months", "failed_path_buckets", "last_error"],
      { workflow, month_list },
    );
  }

  const workflow = {
    input: {
      group_id: "123",
      group_owner_user_id: 1,
      memories_grpc_host: "localhost",
      memories_grpc_port: 9100,
      issue_tracker_grpc_host: "localhost",
      issue_tracker_grpc_port: 9200,
      policy_json: "{}",
    },
  };
  const initialized = initializeFields(
    issueResolutionBatch,
    "initState",
    [
      "review_rows",
      "completed_candidate_ids",
      "canceled_candidate_ids",
      "rejected_candidate_ids",
      "skipped_candidate_ids",
      "applied_candidates",
      "dry_run_candidates",
      "protected_candidates",
      "skipped_candidates",
      "conflicted_candidates",
      "group_changed_candidates",
      "failed_candidates",
    ],
    { workflow },
  );
  // The shared listing worker found no candidate citing a member.
  const review_listing = { manager_contract: {}, manager_store_identity: {}, review_rows: [] };
  return {
    ...initialized,
    review_rows: runStep(issueResolutionBatch, "adoptReviewCandidates", "review_rows", { review_listing }),
  };
}

function emptyOutput(testCase: ProgressCase, bindings: Record<string, unknown>): Record<string, unknown> {
  const counters =
    testCase.workflow === issueResolutionBatch
      ? [
          "candidate_count",
          "processed_count",
          "applied_count",
          "protected_count",
          "skipped_count",
          "conflicted_count",
          "group_changed_count",
          "failed_count",
          "completed_count",
          "canceled_count",
          "rejected_count",
        ]
      : [
          "outcome",
          testCase.processedCount,
          "generated_count",
          "skipped_count",
          "succeeded_count",
          "failed_count",
        ];
  const values: Record<string, unknown> = {};
  for (const key of ["completed", ...counters]) {
    values[key] = outputValue(testCase.workflow, key, bindings);
  }
  return values;
}

describe("summary batch item progress", () => {
  it("does not register the retired summaries pipeline worker", () => {
    expect(llmWorkers).not.toContain("name: memories-summaries-pipeline");
    expect(llmWorkers).not.toContain("workflows/summaries-pipeline/summaries-pipeline.yaml");
  });

  it.each(cases)("reports the current item first in the $name loop", (testCase) => {
    const block = loopBlock(testCase);
    expect(block.includes(`at: ${testCase.index}`)).toBe(true);
    expect(block.includes(`in: "\${ $${testCase.list} }"`)).toBe(true);
    expect(block.indexOf("- reportProgress:")).toBeGreaterThanOrEqual(0);
    expect(block.indexOf("- reportProgress:")).toBeLessThan(block.indexOf(`- ${testCase.firstWorkStep}:`));

    const events = evaluate(progressEventsExpression(testCase), { [testCase.list]: testCase.targets }) as {
      progress_processed: number;
      progress_total: number;
    }[];
    expect(events[0]).toEqual({ progress_processed: 1, progress_total: testCase.targets.length });
    expect(events[events.length - 1]).toEqual({
      progress_processed: testCase.targets.length,
      progress_total: testCase.targets.length,
    });
    expect(progressAt({ ...testCase, targets: [testCase.targets[0]] }, 0)).toEqual({
      processed: 1,
      total: 1,
    });
  });

  it.each(cases)("finishes the empty $name loop with no progress and zero counters", (testCase) => {
    const bindings = emptyState(testCase);
    expect(evaluate(progressEventsExpression(testCase), bindings)).toEqual([]);

    if (testCase.workflow === issueResolutionBatch) {
      expect(emptyOutput(testCase, bindings)).toEqual({
        completed: true,
        candidate_count: 0,
        processed_count: 0,
        applied_count: 0,
        protected_count: 0,
        skipped_count: 0,
        conflicted_count: 0,
        group_changed_count: 0,
        failed_count: 0,
        completed_count: 0,
        canceled_count: 0,
        rejected_count: 0,
      });
      return;
    }

    expect(emptyOutput(testCase, bindings)).toEqual({
      completed: true,
      outcome: "succeeded",
      [testCase.processedCount]: 0,
      generated_count: 0,
      skipped_count: 0,
      succeeded_count: 0,
      failed_count: 0,
    });
  });
});
