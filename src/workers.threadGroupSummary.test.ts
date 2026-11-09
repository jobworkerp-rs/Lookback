import { describe, expect, it } from "vitest";
import memoryDataProto from "../proto/llm_memory/data/common.proto?raw";
import single from "../workers/lang-workers/workers/thread-group-summary/thread-group-summary-single.yaml?raw";
import extractWorkers from "../workers/lookback-issue-extract-workers.yaml?raw";
import periodicWorkflow from "../workers/workflows/lookback-periodic-run.yaml?raw";
import batch from "../workers/workflows/thread-group-summary/thread-group-summary-batch.yaml?raw";

// src tests cannot import scripts/lib (Node-only types); same asserting
// contract as scripts/lib/workflowJq.ts `blockBetween`.
function blockBetween(source: string, startMarker: string, endMarker?: string): string {
  const start = source.indexOf(startMarker);
  expect(start, `missing ${startMarker}`).toBeGreaterThan(-1);
  if (endMarker === undefined) return source.slice(start);
  const end = source.indexOf(endMarker, start + startMarker.length);
  expect(end, `missing ${endMarker} after ${startMarker}`).toBeGreaterThan(start);
  return source.slice(start, end);
}

describe("thread-group-summary-single workflow", () => {
  it("reads the group lineage and writes the generic derived memory kind", () => {
    expect(single).toContain("ThreadGroupService/FindThreadGroupLineage");
    expect(single).toContain("MEMORY_KIND_DERIVED_SUMMARY");
    expect(single).not.toContain("MEMORY_KIND_THREAD_GROUP_SUMMARY");
    expect(single).toContain("thread-group-summary:");
    expect(single).toContain("group_memory_relation:");
    expect(single).toContain('on_group_delete: "GROUP_MEMORY_DELETE_POLICY_DELETE"');
    expect(single.indexOf("ThreadGroupService/LinkThreadGroupMemory")).toBeGreaterThan(0);
    expect(single.indexOf("ThreadGroupService/LinkThreadGroupMemory")).toBeLessThan(
      single.indexOf("MemoryService/Update"),
    );
    expect(single).toContain("membership_snapshot_digest");
    expect(single).toContain("expected_membership_snapshot_digest");
  });

  it("selects live proto members using the gRPC JSON enum and scalar thread ID", () => {
    expect(single).toContain("THREAD_GROUP_MEMBER_STATE_ACTIVE");
    expect(single).toContain(".threadId != null");
    expect(single).toContain("| .threadId | tostring");
    expect(single).not.toContain('(.state // "active") == "active"');
    expect(single).not.toContain(".threadId.value | tostring");
  });

  it("reports a missing member summary as incomplete input", () => {
    expect(single).toContain("missing_member_summary_thread_ids");
    expect(single).toContain('outcome: "incomplete_input"');
    expect(single).toContain("thread-summary-missing");
  });

  it("fails closed when the language prompt was not baked into the worker", () => {
    expect(single).toContain("- validateGroupSummaryPromptContext:");
    expect(single).toContain("$thread_group_summary_system_prompt");
    expect(single).toContain("$thread_group_summary_user_tail");
  });

  it("generates through the named LLM worker with the agent-app summary schema", () => {
    const generation = blockBetween(
      single,
      "- generateSummaryWithRetry:",
      "- fetchLineageBeforeWrite:",
    );
    expect(generation).toContain(`workerName: "\${$workflow.input.llm_worker_name}"`);
    expect(generation).not.toContain("runnerName: LLM");
    expect(generation).not.toContain("ollama");
    for (const field of [
      '"category"',
      '"title"',
      '"summary"',
      '"key_decisions"',
      '"status"',
      '"has_unfinished_issue_candidates"',
      '"source_thread_ids"',
    ]) {
      expect(generation).toContain(field);
    }
    expect(generation).toMatch(/"required": \[[^\]]*"has_unfinished_issue_candidates"/);
    expect(single).not.toContain("summary_model");
    expect(single).not.toContain("ollama_base_url");
  });

  it("keeps the fingerprint that decides freshness unchanged", () => {
    const fingerprint = blockBetween(single, "- buildSummaryFingerprint:", "- evaluateFreshness:");
    for (const field of [
      "group_id: ($group.id.value | tostring)",
      'membership_snapshot_digest: ($group.membershipSnapshotDigest // "")',
      "member_summary_ids: ($member_summary_entries | map(.memory.id.value | tostring) | sort)",
      "member_summary_contents: ($member_summaries | map(tostring))",
      'output_language: ($workflow.input.output_language // "ja")',
      "aggregation_mode: $aggregation_mode",
      'summary_version: "1.0"',
    ]) {
      expect(fingerprint).toContain(field);
    }
    expect(single).toContain("- skipFreshSummary:");
  });

  it("dispatches cross-thread resolution once, after recording a generated outcome", () => {
    expect(single.match(/lookback-thread-group-issue-resolution/g)).toHaveLength(1);
    expect(single).not.toContain("lookback-issue-extract");
    expect(single.indexOf("- dispatchCrossThreadResolution:")).toBeGreaterThan(
      single.indexOf("- recordOutcome:"),
    );
    const dispatch = blockBetween(single, "- dispatchCrossThreadResolution:", "\noutput:");
    expect(dispatch).toContain('$outcome == "generated"');
    expect(dispatch).toContain('(try (($workflow.input.policy_json // "") | fromjson)) // {}');
    expect(dispatch.indexOf('== "APPLY"')).toBeLessThan(
      dispatch.indexOf("workerName: lookback-thread-group-issue-resolution"),
    );
    for (const field of ["policy_json", "issue_tracker_grpc_host", "issue_tracker_grpc_port"]) {
      expect(dispatch).toContain(field);
    }
  });

  it("enqueues cross-thread resolution without waiting, under its own 24-hour job deadline", () => {
    const dispatch = blockBetween(single, "- dispatchCrossThreadResolution:", "\noutput:");
    const invoke = blockBetween(dispatch, "- invokeCrossThreadResolution:", "catch:");
    expect(invoke).toMatch(/run:\n\s+await: false\n/);
    expect(invoke).toMatch(/timeout:\n\s+after:\n\s+hours: 24\n/);
    expect(invoke).not.toContain("fromjson");
    expect(dispatch).toContain(`resolution_dispatched: "\${ true }"`);
    expect(single).toContain(`resolution_dispatched: "\${ false }"`);
    expect(single).not.toContain("resolution_result");
    const output = blockBetween(single, "\noutput:");
    expect(output).toContain(`resolution_dispatched: "\${ $resolution_dispatched }"`);
    expect(output).toContain(`resolution_error: "\${ $resolution_error }"`);
  });

  it("runs cross-thread resolution on a channel the awaited thread issue workers do not share", () => {
    const registration = blockBetween(
      extractWorkers,
      "  - name: lookback-thread-group-issue-resolution\n",
      "\n  - name:",
    );
    expect(registration).toContain("channel: issue_resolution_group");
    expect(extractWorkers.match(/channel: issue_resolution_group/g)).toHaveLength(1);
  });

  it("records a changed snapshot as stale instead of current", () => {
    expect(single).toContain("$fresh_lineage.group.membershipSnapshotDigest");
    expect(single).toContain(`publication_state: "\${ \\"stale\\" }"`);
    expect(single).toContain(
      `outcome: "\${ if $publication_state == \\"current\\" then \\"generated\\" else \\"stale\\" end }"`,
    );
  });
});

describe("thread-group-summary-batch workflow", () => {
  it("selects groups by group owner and refreshes each member through the thread single", () => {
    expect(batch).toContain("ThreadGroupService/FindThreadGroupList");
    expect(batch).toContain("include_inactive: false, user_id: $group_owner_user_id");
    const refresh = blockBetween(
      batch,
      "- refreshMemberSummaries:",
      "- dedupeInsufficientMembers:",
    );
    expect(refresh).toContain(
      `workerName: '\${ "memories-thread-summary-single-" + ($workflow.input.output_language // "ja") }'`,
    );
    expect(refresh).toContain("user_id: $member_target.user_id");
    expect(refresh).toContain("thread_id: $member_target.thread_id");
    expect(batch.indexOf("- buildMemberRefreshTargets:")).toBeLessThan(
      batch.indexOf("- refreshMemberSummaries:"),
    );
    expect(batch.indexOf("- refreshMemberSummaries:")).toBeLessThan(
      batch.indexOf("- summarizeEachGroup:"),
    );
  });

  it("publishes member refresh progress before each member's single call", () => {
    const refresh = blockBetween(
      batch,
      "- refreshMemberSummaries:",
      "- dedupeInsufficientMembers:",
    );
    expect(refresh).toContain("at: member_target_index");
    expect(refresh.indexOf("- reportProgress:")).toBeGreaterThan(-1);
    expect(refresh.indexOf("- reportProgress:")).toBeLessThan(
      refresh.indexOf("- invokeMemberSingle:"),
    );
    // Each call is one thread, bounded by the run/call default.
    expect(refresh).not.toContain("timeout:");
  });

  it("requires an explicit positive group owner for direct and scheduled runs", () => {
    expect(batch).toMatch(/required: \[[^\]]*group_owner_user_id/);
    expect(batch).toMatch(/group_owner_user_id:[\s\S]*?minimum: 1/);
    expect(batch).not.toContain("defaults to legacy user_id");
    expect(batch).toContain("$workflow.input.group_owner_user_id");
    expect(batch.indexOf("- validateGroupOwner:")).toBeLessThan(
      batch.indexOf("- verifyGroupOwnerCapability:"),
    );
  });

  it("keeps summarizing the remaining groups after one group fails", () => {
    expect(batch).toMatch(/- summarizeEachGroup:[\s\S]+?onError: continue\n\s+do:/);
  });

  it("publishes group progress at the start of each iteration for the import toast", () => {
    const groupLoop = blockBetween(batch, "- summarizeEachGroup:", "- summarizeGroup:");
    expect(groupLoop).toContain("at: group_index");
    expect(groupLoop).toContain("- reportProgress:");
    expect(groupLoop).toContain(`progress_processed: "\${ $group_index + 1 }"`);
    expect(groupLoop).toContain(`progress_total: "\${ $target_groups | length }"`);
  });

  it("aggregates each group through the language-specific single worker", () => {
    const invoke = blockBetween(batch, "- invokeSingle:", "- rejectIncompleteInput:");
    expect(invoke).toContain(
      `workerName: '\${ "memories-thread-group-summary-single-" + ($workflow.input.output_language // "ja") }'`,
    );
    for (const field of [
      "group_owner_user_id: $group_owner_user_id",
      "llm_worker_name:",
      "issue_tracker_grpc_host:",
      "issue_tracker_grpc_port:",
      "policy_json:",
      "insufficient_message_thread_ids:",
    ]) {
      expect(invoke).toContain(field);
    }
    expect(batch).not.toContain("summary_model");
    expect(batch).not.toContain("ollama_base_url");
    expect(batch).not.toContain("lookback-thread-group-issue-resolution");
    expect(batch).not.toContain("lookback-issue-extract");
  });

  it("exposes the singles' resolution dispatches and failures in group-scoped lists", () => {
    expect(batch).toContain(`resolution_failures: "\${ [] }"`);
    expect(batch).toContain(`resolution_dispatched_groups: "\${ [] }"`);
    expect(batch.indexOf("- recordResolution:")).toBeGreaterThan(batch.indexOf("- recordOutcome:"));
    expect(batch).toContain(`resolution_failures: "\${ $resolution_failures }"`);
    expect(batch).toContain(`resolution_dispatched_groups: "\${ $resolution_dispatched_groups }"`);
  });

  it("accounts for every group outcome and separates incomplete input from failure", () => {
    for (const field of [
      "failed_groups: >-",
      "stale_groups",
      "incomplete_groups",
      "incomplete_input",
      "failed_count:",
      "expected_group_count:",
      "($target_groups | length)",
    ]) {
      expect(batch).toContain(field);
    }
  });

  it("relays policy and issue tracker coordinates through periodic group summaries", () => {
    const periodicDispatch = blockBetween(
      periodicWorkflow,
      "- threadGroupForPeriodic:",
      "- threadGroupPeriodicFatalCheck:",
    );

    expect(periodicDispatch).toContain("workerName: memories-thread-group-summary-batch");
    expect(periodicDispatch).toContain("group_owner_user_id: 1");
    expect(periodicDispatch).not.toMatch(/^\s*user_id:\s*1,/m);
    // The scheduler's runtime carries the tracker: a remote jobworkerp has no
    // app-set environment to fall back on.
    for (const field of ["host", "port", "tls"]) {
      expect(periodicDispatch).toContain(
        `issue_tracker_grpc_${field}: ($workflow.input.runtime.issue_tracker_grpc_${field}`,
      );
    }
    expect(periodicDispatch).toContain('policy_json: ($workflow.input.runtime.policy_json // "")');
  });
});

it("keeps the former group kind as a deprecated protobuf alias for numeric 8", () => {
  expect(memoryDataProto).toContain("option allow_alias = true;");
  expect(memoryDataProto).toMatch(/MEMORY_KIND_DERIVED_SUMMARY\s*=\s*8\s*;/);
  expect(memoryDataProto).toMatch(
    /MEMORY_KIND_THREAD_GROUP_SUMMARY\s*=\s*8\s*\[deprecated\s*=\s*true\]/,
  );
});
