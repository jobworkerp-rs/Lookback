import { describe, expect, it } from "vitest";
import groupWorkflow from "../workers/workflows/thread-group-summary/thread-group-summary-batch.yaml?raw";
import threadBatchWorkflow from "../workers/workflows/thread-summary/thread-summary-batch.yaml?raw";
import groupSingleWorkflow from "../workers/lang-workers/workers/thread-group-summary/thread-group-summary-single.yaml?raw";
import threadSingleWorkflow from "../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml?raw";
import { blockBetween, evaluate, runStep, stepExpression } from "./lib/workflowJq";

const INSUFFICIENT_SKIP_REASON = "skipped: not enough messages";

const groupState = (id: string, extra: Record<string, unknown> = {}) => ({
  group: { id: { value: id } },
  generated_groups: [],
  stale_groups: [],
  skipped_groups: [],
  resolution_dispatched_groups: [],
  resolution_failures: [],
  ...extra,
});

describe("thread-group-summary-batch refreshes member summaries one thread at a time", () => {
  const refreshPhase = blockBetween(
    groupWorkflow,
    "- refreshMemberSummaries:",
    "- dedupeInsufficientMembers:",
  );
  const targets = (entries: unknown[]) =>
    runStep(groupWorkflow, "buildMemberRefreshTargets", "member_refresh_targets", {
      owner_member_entries: entries,
    });

  it("refreshes a thread shared by several groups once, ordered by owner", () => {
    expect(
      targets([
        { user_id: 7, thread_id: "3" },
        { user_id: 1, thread_id: "1" },
        { user_id: 7, thread_id: "2" },
        { user_id: 7, thread_id: "3" },
      ]),
    ).toEqual([
      { user_id: 1, thread_id: "1" },
      { user_id: 7, thread_id: "2" },
      { user_id: 7, thread_id: "3" },
    ]);
    expect(targets([])).toEqual([]);
  });

  it("reports the member being refreshed among all member threads", () => {
    const member_refresh_targets = [
      { user_id: 1, thread_id: "1" },
      { user_id: 7, thread_id: "2" },
      { user_id: 7, thread_id: "3" },
    ];
    const at = (member_target_index: number) => [
      runStep(refreshPhase, "reportProgress", "progress_processed", {
        member_refresh_targets,
        member_target_index,
      }),
      runStep(refreshPhase, "reportProgress", "progress_total", {
        member_refresh_targets,
        member_target_index,
      }),
    ];
    expect(at(0)).toEqual([1, 3]);
    expect(at(2)).toEqual([3, 3]);
  });

  const refresh = (result: unknown) => {
    const member_refreshed = runStep(groupWorkflow, "classifyMemberRefresh", "member_refreshed", {
      member_single_result: result,
    });
    const bindings = {
      member_single_result: result,
      member_refreshed,
      member_target: { user_id: 7, thread_id: "2" },
      insufficient_message_thread_ids: ["9"],
      refresh_failed_thread_ids: [],
      refresh_error: null,
    };
    const collect = (key: string) =>
      runStep(groupWorkflow, "collectMemberRefreshResult", key, bindings);
    return {
      insufficient: collect("insufficient_message_thread_ids"),
      failed: collect("refresh_failed_thread_ids"),
      error: collect("refresh_error"),
    };
  };

  it("counts a generated or up-to-date member as refreshed", () => {
    for (const result of [
      { completed: true, skipped: false },
      { completed: false, skipped: true, skip_reason: "skipped: already up to date" },
      { completed: false, skipped: true, skip_reason: "skipped: thread not found" },
    ]) {
      expect(refresh(result)).toEqual({ insufficient: ["9"], failed: [], error: null });
    }
  });

  it("drops a member whose thread is too short to summarize", () => {
    expect(
      refresh({ completed: false, skipped: true, skip_reason: INSUFFICIENT_SKIP_REASON }),
    ).toEqual({ insufficient: ["9", "2"], failed: [], error: null });
  });

  it("fails a member whose single returned neither completed nor skipped", () => {
    const outcome = refresh({ completed: false, skipped: false });
    expect(outcome.failed).toEqual(["2"]);
    expect(outcome.insufficient).toEqual(["9"]);
    expect(outcome.error).toEqual(expect.stringContaining("neither completed nor skipped"));
  });

  it("fails only the member whose single raised", () => {
    const bindings = {
      member_target: { user_id: 7, thread_id: "2" },
      refresh_failed_thread_ids: ["1"],
      refresh_error: null,
      error: { title: "boom" },
    };
    expect(
      runStep(refreshPhase, "recordMemberRefreshFailure", "refresh_failed_thread_ids", bindings),
    ).toEqual(["1", "2"]);
    expect(runStep(refreshPhase, "recordMemberRefreshFailure", "refresh_error", bindings)).toBe(
      JSON.stringify({ title: "boom" }),
    );
  });

  it("rejects only groups that contain a member whose refresh failed", () => {
    const bindings = (memberIds: string[]) => ({
      group: { id: { value: "10" } },
      group_member_thread_ids: { "10": memberIds },
      refresh_failed_thread_ids: ["2"],
    });
    expect(runStep(groupWorkflow, "rejectRefreshFailedMembers", "if", bindings(["1", "2"]))).toBe(
      true,
    );
    expect(runStep(groupWorkflow, "rejectRefreshFailedMembers", "if", bindings(["1", "3"]))).toBe(
      false,
    );
  });
});

describe("thread-group-summary-batch records single outcomes", () => {
  it("turns incomplete input and a missing outcome into failures", () => {
    expect(
      runStep(groupWorkflow, "rejectIncompleteInput", "if", {
        single_result: { outcome: "incomplete_input", completed: false },
      }),
    ).toBe(true);
    expect(
      runStep(groupWorkflow, "rejectLogicalFailure", "if", { single_result: { completed: false } }),
    ).toBe(true);
    expect(
      runStep(groupWorkflow, "rejectLogicalFailure", "if", {
        single_result: { outcome: "skipped", completed: true },
      }),
    ).toBe(false);
  });

  it.each([
    ["generated", "generated_groups"],
    ["stale", "stale_groups"],
    ["skipped", "skipped_groups"],
  ])("records a %s group under %s only", (outcome, listName) => {
    const state = groupState("10", { single_result: { outcome, completed: true } });
    for (const name of ["generated_groups", "stale_groups", "skipped_groups"]) {
      expect(runStep(groupWorkflow, "recordOutcome", name, state)).toEqual(
        name === listName ? ["10"] : [],
      );
    }
  });

  it("rebuilds group-scoped resolution dispatches and failures from the single", () => {
    const dispatched = groupState("10", {
      single_result: { outcome: "generated", resolution_dispatched: true, resolution_error: null },
    });
    expect(
      runStep(groupWorkflow, "recordResolution", "resolution_dispatched_groups", dispatched),
    ).toEqual(["10"]);
    expect(runStep(groupWorkflow, "recordResolution", "resolution_failures", dispatched)).toEqual([]);

    const notDispatched = groupState("12", {
      single_result: { outcome: "skipped", resolution_dispatched: false, resolution_error: null },
    });
    expect(
      runStep(groupWorkflow, "recordResolution", "resolution_dispatched_groups", notDispatched),
    ).toEqual([]);

    const failure = groupState("11", {
      single_result: { outcome: "generated", resolution_dispatched: false, resolution_error: "boom" },
    });
    expect(
      runStep(groupWorkflow, "recordResolution", "resolution_dispatched_groups", failure),
    ).toEqual([]);
    expect(runStep(groupWorkflow, "recordResolution", "resolution_failures", failure)).toEqual([
      { group_id: "11", error: "boom" },
    ]);
  });

  it.each([
    ["thread-refresh-failed", true],
    ["thread-summary-missing", true],
    ["snapshot-changed", false],
  ])("classifies a %s failure as incomplete input: %s", (kind, expected) => {
    expect(
      runStep(groupWorkflow, "classifyFailure", "is_incomplete_input", {
        error: { type: `https://memories.local/errors/thread-group-summary/${kind}` },
      }),
    ).toBe(expected);
  });
});

describe("thread-summary-batch reports threads skipped for insufficient messages", () => {
  const predicate = () => stepExpression(threadBatchWorkflow, "recordInsufficientMessages", "if");

  it.each([
    [{ completed: false, skipped: true, skip_reason: INSUFFICIENT_SKIP_REASON }, true],
    [{ completed: false, skipped: true, skip_reason: "skipped: already up to date" }, false],
    [{ completed: false, skipped: true, skip_reason: "skipped: thread not found" }, false],
    [{ completed: true, skipped: false, skip_reason: null }, false],
    [{ completed: true }, false],
  ])("evaluates %j as %s", (singleResult, expected) => {
    expect(evaluate(predicate(), { single_result: singleResult })).toBe(expected);
  });

  it("matches the skip reason emitted by thread-summary-single", () => {
    expect(threadSingleWorkflow).toContain(`result: "${INSUFFICIENT_SKIP_REASON}"`);
    expect(predicate()).toContain(`"${INSUFFICIENT_SKIP_REASON}"`);
  });

  it("exposes the collected threads in the batch output", () => {
    const output = threadBatchWorkflow.slice(threadBatchWorkflow.lastIndexOf("\noutput:"));
    expect(output).toContain("insufficient_message_threads: ${ $insufficient_message_threads }");
  });
});

describe("thread-group-summary-batch scopes per-group inputs", () => {
  it("dedupes the short-member list once before the group loop", () => {
    expect(
      runStep(groupWorkflow, "dedupeInsufficientMembers", "insufficient_message_thread_ids", {
        insufficient_message_thread_ids: ["3", "1", "3"],
      }),
    ).toEqual(["1", "3"]);
  });

  it("sends each single only its own members that lacked messages", () => {
    const input = runStep(groupWorkflow, "invokeSingle", "input", {
      workflow: { input: { memories_grpc_host: "h", memories_grpc_port: 1, memories_grpc_tls: false } },
      group: { id: { value: "10" } },
      group_owner_user_id: 1,
      group_member_thread_ids: { "10": ["1", "2"], "11": ["3"] },
      insufficient_message_thread_ids: ["2", "3"],
    });
    expect(JSON.parse(input as string).insufficient_message_thread_ids).toEqual(["2"]);
  });
});

describe("thread-group-summary snapshot guard", () => {
  // The batch and the single must agree on when a supplied digest is checked.
  const cases: [string, Record<string, unknown>, boolean][] = [
    ["absent digest", {}, false],
    ["empty digest", { expected_membership_snapshot_digest: "" }, false],
    ["matching digest", { expected_membership_snapshot_digest: "d1" }, false],
    ["changed digest", { expected_membership_snapshot_digest: "d0" }, true],
  ];
  for (const [label, input, raises] of cases) {
    it(`raises only for a changed digest (${label})`, () => {
      expect(
        runStep(groupWorkflow, "validateTargetSnapshot", "if", {
          workflow: { input },
          group: { membershipSnapshotDigest: "d1" },
        }),
      ).toBe(raises);
      expect(
        runStep(groupSingleWorkflow, "validateTargetSnapshot", "if", {
          workflow: { input },
          group: { membershipSnapshotDigest: "d1" },
        }),
      ).toBe(raises);
    });
  }
});
