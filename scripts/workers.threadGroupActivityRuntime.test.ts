import { describe, expect, it } from "vitest";
import periodicRun from "../workers/workflows/lookback-periodic-run.yaml?raw";
import groupBatch from "../workers/workflows/thread-group-summary/thread-group-summary-batch.yaml?raw";
import { blockBetween, runStep as run } from "./lib/workflowJq";

const WINDOW = { activity_after_ms: 1_000, activity_before_ms: 2_000 };

describe("ThreadGroup activity window", () => {
  it.each([
    [{ ...WINDOW }, true],
    [{ ...WINDOW, target_group_id: "7" }, false],
    [{ activity_after_ms: 1_000 }, false],
    [{}, false],
  ])("browses by activity only for a full, untargeted window (%#)", (input, expected) => {
    expect(run(groupBatch, "resolveGroupScope", "browse_by_activity", { workflow: { input } })).toBe(expected);
  });

  it("requests one page with an overlap filter and continues with the cursor", () => {
    const bindings = { workflow: { input: WINDOW }, group_owner_user_id: 1 };
    const first = JSON.parse(run(groupBatch, "readGroupPage", "json_body", { ...bindings, group_page_token: "" }) as string);
    expect(first).toEqual({
      user_id: 1,
      page_size: 100,
      browse_filter: { last_message_after: 1_000, first_message_before: 2_000 },
    });
    const next = JSON.parse(
      run(groupBatch, "readGroupPage", "json_body", { ...bindings, group_page_token: "cursor-2" }) as string,
    );
    expect(next.page_token).toBe("cursor-2");
  });

  it("reads groups and the next cursor from a page response", () => {
    const response = {
      jsonBody: JSON.stringify({ results: [{ group: { id: { value: "5" } } }], nextPageToken: "cursor-2" }),
    };
    expect(run(groupBatch, "readGroupPage", "group_page_groups", {}, response)).toEqual([{ id: { value: "5" } }]);
    expect(run(groupBatch, "readGroupPage", "group_page_next_token", {}, response)).toBe("cursor-2");
    const last = { jsonBody: JSON.stringify({ results: [] }) };
    expect(run(groupBatch, "readGroupPage", "group_page_next_token", {}, last)).toBe("");
  });

  it("collects pages, stops after the last one and drops duplicates once", () => {
    const state = {
      all_groups: [{ id: { value: "5" } }],
      group_page_groups: [{ id: { value: "5" } }, { id: { value: "6" } }],
      group_page_next_token: "",
    };
    const collected = run(groupBatch, "collectGroupPage", "all_groups", state);
    expect(run(groupBatch, "collectGroupPage", "group_pages_exhausted", state)).toBe(true);
    expect(run(groupBatch, "dedupeBrowsedGroups", "all_groups", { all_groups: collected })).toEqual([
      { id: { value: "5" } },
      { id: { value: "6" } },
    ]);
  });
});

describe("periodic activity window", () => {
  const periodicInput = (task: Record<string, unknown>, extra: Record<string, unknown> = {}) => ({
    workflow: {
      input: {
        task,
        runtime: {
          memories_grpc_host: "h",
          memories_grpc_port: 1,
          memories_grpc_tls: false,
          llm_worker_name: "w",
        },
      },
    },
    ...extra,
  });
  const windowStart = (task: Record<string, unknown>) =>
    run(periodicRun, "buildPeriodicActivityWindow", "periodic_activity_after_ms", periodicInput(task)) as number;
  const stageInput = (step: string, task: Record<string, unknown>, afterMs: number) =>
    JSON.parse(
      run(
        periodicRun,
        step,
        "input",
        periodicInput(task, {
          periodic_activity_after_ms: afterMs,
          source_filter_labels: [],
          source_filter_match_mode: "LABEL_ANY",
        }),
      ) as string,
    );

  it("starts the task's lookback days plus a day before now", () => {
    const before = Date.now();
    const start = windowStart({ lookback_days: 7 });
    const after = Date.now();
    const eightDays = 8 * 86_400_000;
    expect(start).toBeGreaterThanOrEqual(before - eightDays - 1_000);
    expect(start).toBeLessThanOrEqual(after - eightDays);
  });

  it("covers the import's local-midnight start for a same-day task", () => {
    const start = windowStart({ lookback_days: 0 });
    expect(Date.now() - start).toBeGreaterThanOrEqual(86_400_000);
  });

  it("is computed before the thread summary stage that uses it", () => {
    blockBetween(periodicRun, "- buildPeriodicActivityWindow:", "- threadSummaryForTask:");
  });

  it("limits the periodic thread summary to threads active within the window", () => {
    const input = stageInput("runThreadBatch", { lookback_days: 7 }, 1_234);
    expect(input.last_message_after_ms).toBe(1_234);
  });

  it("browses ThreadGroups over the same window up to now", () => {
    const before = Date.now();
    const input = stageInput("threadGroupForPeriodic", { lookback_days: 7 }, 1_234);
    const after = Date.now();
    expect(input.activity_after_ms).toBe(1_234);
    expect(input.activity_before_ms).toBeGreaterThan(before - 1_000);
    expect(input.activity_before_ms).toBeLessThanOrEqual(after + 1_000);
  });
});
