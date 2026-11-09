import { describe, expect, it } from "vitest";
import single from "../workers/lang-workers/workers/thread-group-summary/thread-group-summary-single.yaml?raw";
import { evaluate, runStep, stepExpression } from "./lib/workflowJq";

const summaryEntry = (threadId: string, content = `summary of ${threadId}`) => ({
  memory: {
    id: { value: `9${threadId}` },
    data: { externalId: `summary:${threadId}`, content },
  },
});

function resolveSources(
  memberThreadIds: string[],
  insufficient: string[],
  threadSummaries: unknown[],
) {
  const summaryByThreadId = runStep(single, "indexMemberSummaries", "summary_by_thread_id", {
    thread_summaries: threadSummaries,
  });
  const sources = runStep(single, "excludeInsufficientMessageMembers", "source_member_thread_ids", {
    workflow: { input: { insufficient_message_thread_ids: insufficient } },
    member_thread_ids: memberThreadIds,
    summary_by_thread_id: summaryByThreadId,
  }) as string[];
  const collected = { source_member_thread_ids: sources, summary_by_thread_id: summaryByThreadId };
  return {
    sources,
    entries: runStep(single, "collectMemberSummaries", "member_summary_entries", collected),
    missing: runStep(
      single,
      "collectMemberSummaries",
      "missing_member_summary_thread_ids",
      collected,
    ) as string[],
  };
}

describe("thread-group-summary-single member summary selection", () => {
  it("drops an unsummarized short member and aggregates the rest", () => {
    const result = resolveSources(["1", "2"], ["2"], [summaryEntry("1")]);
    expect(result.sources).toEqual(["1"]);
    expect(result.entries).toEqual([summaryEntry("1")]);
    expect(result.missing).toEqual([]);
  });

  it("keeps a short member that still has a persisted summary", () => {
    const result = resolveSources(["1", "2"], ["2"], [summaryEntry("1"), summaryEntry("2")]);
    expect(result.sources).toEqual(["1", "2"]);
    expect(result.missing).toEqual([]);
  });

  it("reports members missing a summary for any other reason", () => {
    const result = resolveSources(["1", "2", "3"], ["3"], [summaryEntry("1")]);
    expect(result.sources).toEqual(["1", "2"]);
    expect(result.missing).toEqual(["2"]);
    expect(
      runStep(single, "reportMissingMemberSummaries", "if", {
        missing_member_summary_thread_ids: result.missing,
      }),
    ).toBe(true);
  });

  it("yields no sources when every member is short and unsummarized", () => {
    const result = resolveSources(["1", "2"], ["1", "2"], []);
    expect(result.sources).toEqual([]);
    expect(result.entries).toEqual([]);
    expect(result.missing).toEqual([]);
  });

  it("rejects an active member whose Thread owner is missing", () => {
    const lineage = {
      members: [
        { state: "THREAD_GROUP_MEMBER_STATE_ACTIVE", threadId: "1", userId: "7" },
        { state: "THREAD_GROUP_MEMBER_STATE_ACTIVE", threadId: "2", userId: null },
      ],
    };
    const missing = runStep(single, "collectMemberThreadIds", "missing_member_owner_thread_ids", {
      lineage,
    });
    expect(missing).toEqual(["2"]);
    expect(
      runStep(single, "rejectMissingMemberOwner", "if", {
        missing_member_owner_thread_ids: missing,
      }),
    ).toBe(true);
  });
});

describe("thread-group-summary-single aggregation mode", () => {
  it("passes one JSON member summary through and sends several to the LLM", () => {
    const one = [summaryEntry("1", JSON.stringify({ title: "t", summary: "s" }))];
    const passthrough = runStep(single, "preparePassthroughSummary", "passthrough_summary_json", {
      member_summary_entries: one,
    });
    expect(passthrough).toEqual({ title: "t", summary: "s" });
    expect(runStep(single, "preparePassthroughSummary", "if", { member_summary_entries: one })).toBe(
      true,
    );
    expect(
      runStep(single, "preparePassthroughSummary", "if", {
        member_summary_entries: [summaryEntry("1"), summaryEntry("2")],
      }),
    ).toBe(false);
    expect(
      runStep(single, "resolveAggregationMode", "aggregation_mode", {
        passthrough_summary_json: passthrough,
      }),
    ).toBe("passthrough");
    expect(
      runStep(single, "resolveAggregationMode", "aggregation_mode", {
        passthrough_summary_json: null,
      }),
    ).toBe("llm");
  });

  it("falls back to the LLM when the single member summary is not a JSON object", () => {
    expect(
      runStep(single, "preparePassthroughSummary", "passthrough_summary_json", {
        member_summary_entries: [summaryEntry("1", "plain text")],
      }),
    ).toBeNull();
  });
});

describe("thread-group-summary-single cross-thread issue resolution", () => {
  const dispatchIf = () => stepExpression(single, "dispatchCrossThreadResolution", "if");
  const apply = JSON.stringify({ resolution_mode: "APPLY" });

  it.each([
    ["generated", apply, true],
    ["generated", JSON.stringify({ resolution_mode: "DRY_RUN" }), false],
    ["generated", "", false],
    ["generated", "{not json", false],
    ["stale", apply, false],
    ["skipped", apply, false],
    ["incomplete_input", apply, false],
  ])("dispatches for outcome=%s policy=%s: %s", (outcome, policy, expected) => {
    expect(
      evaluate(dispatchIf(), { outcome, workflow: { input: { policy_json: policy } } }),
    ).toBe(expected);
  });

  it("forwards the group, snapshot, policy and tracker coordinates", () => {
    const input = JSON.parse(
      runStep(single, "invokeCrossThreadResolution", "input", {
        group: { id: { value: 42 }, membershipSnapshotDigest: "d42" },
        workflow: {
          input: {
            group_owner_user_id: 7,
            memories_grpc_host: "m",
            memories_grpc_port: 9100,
            memories_grpc_tls: false,
            issue_tracker_grpc_host: "it",
            issue_tracker_grpc_port: 9200,
            policy_json: apply,
            llm_worker_name: "memories-llm",
            output_language: "en",
          },
        },
      }) as string,
    );
    expect(input).toEqual({
      group_id: "42",
      group_owner_user_id: 7,
      expected_membership_snapshot_digest: "d42",
      memories_grpc_host: "m",
      memories_grpc_port: 9100,
      memories_grpc_tls: false,
      issue_tracker_grpc_host: "it",
      issue_tracker_grpc_port: 9200,
      issue_tracker_grpc_tls: false,
      policy_json: apply,
      llm_worker_name: "memories-llm",
      output_language: "en",
    });
  });

  it("isolates an enqueue failure from the summary outcome", () => {
    expect(
      runStep(single, "recordResolutionFailure", "resolution_error", { error: { title: "boom" } }),
    ).toBe(JSON.stringify({ title: "boom" }));
    expect(
      runStep(single, "recordResolutionFailure", "resolution_dispatched", { error: { title: "boom" } }),
    ).toBe(false);
    const output = single.slice(single.lastIndexOf("\noutput:"));
    expect(output).toContain('resolution_dispatched: "${ $resolution_dispatched }"');
    expect(output).toContain('resolution_error: "${ $resolution_error }"');
    expect(output).not.toMatch(/completed:.*resolution/);
  });
});
