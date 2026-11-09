import { describe, expect, it } from "vitest";
import type {
  ThreadGroupListItem,
  ThreadGroupMemberView,
  ThreadGroupUnresolvedView,
  ThreadRelationView,
} from "@/types/api";
import {
  fetchThreadGroupSummaries,
  groupDisplayTitle,
  inferredCandidates,
  inferredTreeRelations,
  isThreadGroupSnapshotChangedError,
  isThreadGroupUnsupportedError,
  liveThreadId,
  orderGroupMembers,
  relatedGroups,
  threadGroupSummaryState,
  threadGroupTreeProjection,
} from "./threadGroupView";

function group(overrides: Partial<ThreadGroupListItem> = {}): ThreadGroupListItem {
  return {
    id: "7",
    user_id: 7,
    group_canonical_key: "group-7",
    title: null,
    display_name: "無題のスレッドグループ",
    status: "active",
    grouping_authority: "reconciler",
    redirect_to_group_id: null,
    latest_activity_at_ms: null,
    root_thread_id: null,
    root_thread_canonical_key: null,
    root_display: null,
    active_member_count: 0,
    deleted_member_count: 0,
    unresolved_count: 0,
    membership_snapshot_digest: "digest",
    ...overrides,
  };
}

function member(threadId: string | null, state = "active", userId = 1): ThreadGroupMemberView {
  return {
    group_id: "7",
    user_id: userId,
    thread_id: threadId,
    thread_canonical_key: threadId ? `thread-${threadId}` : "deleted-thread",
    source: "codex",
    role: "member",
    state,
    provenance: "reconciler",
    identity_scope: null,
    native_id: null,
    deleted_at_ms: state === "deleted" ? 10 : null,
    display: null,
  };
}

function relation(parent: string, child: string): ThreadRelationView {
  return {
    id: "1",
    parent_thread_id: parent,
    child_thread_id: child,
    parent_thread_canonical_key: `thread-${parent}`,
    child_thread_canonical_key: `thread-${child}`,
    parent_user_id: 1,
    child_user_id: 2,
    relation_type: "continuation",
    state: "active",
    selection_basis: "source_exact",
    source_confidence: "exact",
    selected_observation_id: null,
    selected_operator_decision_id: null,
  };
}

describe("thread group view helpers", () => {
  it("classifies a changed cursor snapshot as restartable", () => {
    expect(isThreadGroupSnapshotChangedError(new Error("aborted: snapshot_changed: restart"))).toBe(
      true,
    );
    expect(isThreadGroupSnapshotChangedError(new Error("invalid page token"))).toBe(false);
  });
  it("prefers an explicit title and falls back to the root description", () => {
    expect(groupDisplayTitle(group({ title: "Project" }))).toBe("Project");
    expect(
      groupDisplayTitle(
        group({
          root_display: {
            thread_id: "3",
            thread_canonical_key: "thread-3",
            description: "Root work",
            source: "codex",
            created_at_ms: null,
            last_message_at_ms: null,
            deleted_at_ms: null,
          },
        }),
      ),
    ).toBe("Root work");
  });

  it("uses only the bracketed root summary title", () => {
    expect(
      groupDisplayTitle(
        group({
          root_display: {
            thread_id: "3",
            thread_canonical_key: "thread-3",
            description: "【Migration review】 ## Purpose\nLong markdown summary",
            source: "codex",
            created_at_ms: null,
            last_message_at_ms: null,
            deleted_at_ms: null,
          },
        }),
      ),
    ).toBe("Migration review");
  });

  it("orders live members by active relations and appends placeholders", () => {
    const members = [member("3"), member("1"), member("2"), member(null, "deleted")];
    expect(members.map(liveThreadId)).toEqual(["3", "1", "2", null]);
    expect(orderGroupMembers(members, [relation("1", "3")]).map((item) => item.thread_id)).toEqual([
      "1",
      "3",
      "2",
      null,
    ]);
  });

  it("keeps members and relation endpoints from different owners in the same group", () => {
    const mixedOwnerMembers = [
      member("2", "active", 2),
      member("1", "active", 1),
      member(null, "deleted", 2),
    ];
    const crossOwnerRelation = {
      ...relation("1", "2"),
      parent_user_id: 1,
      child_user_id: 2,
    };

    const projection = threadGroupTreeProjection(mixedOwnerMembers, [crossOwnerRelation], []);

    expect(projection.members.map(({ thread_id, user_id }) => [thread_id, user_id])).toEqual([
      ["1", 1],
      ["2", 2],
      [null, 2],
    ]);
    expect(projection.relationByChild.get("2")).toMatchObject({
      parent_user_id: 1,
      child_user_id: 2,
    });
  });

  it("does not make deleted or unresolved members selectable", () => {
    expect(liveThreadId(member("3"))).toBe("3");
    expect(liveThreadId(member("3", "deleted"))).toBeNull();
    expect(liveThreadId(member(null))).toBeNull();
  });

  it("recognizes backend unsupported errors without hiding other failures", () => {
    expect(
      isThreadGroupUnsupportedError(
        new Error("この memories バージョンはスレッドグループ表示に未対応です"),
      ),
    ).toBe(true);
    expect(isThreadGroupUnsupportedError(new Error("request failed"))).toBe(false);
    expect(isThreadGroupUnsupportedError(null)).toBe(false);
  });

  it("prioritizes generation and recovery over the current digest state", () => {
    const latest = {
      memory_id: "10",
      external_id: "thread-group-summary:7",
      content_json: JSON.stringify({ title: "Group" }),
      metadata: JSON.stringify({ membership_snapshot_digest: "digest" }),
      updated_at_ms: 10,
      membership_snapshot_digest: "digest",
    };
    expect(
      threadGroupSummaryState({
        group: group(),
        summary: latest,
        generating: false,
        failed: false,
      }),
    ).toBe("latest");
    expect(
      threadGroupSummaryState({
        group: group({ membership_snapshot_digest: "new-digest" }),
        summary: latest,
        generating: false,
        failed: false,
      }),
    ).toBe("stale");
    expect(
      threadGroupSummaryState({ group: group(), summary: null, generating: false, failed: false }),
    ).toBe("absent");
    expect(
      threadGroupSummaryState({ group: group(), summary: latest, generating: false, failed: true }),
    ).toBe("failed");
    expect(
      threadGroupSummaryState({ group: group(), summary: latest, generating: true, failed: true }),
    ).toBe("generating");
  });

  it("shows each active cross-group relation once and hides internal or inactive relations", () => {
    expect(
      relatedGroups("7", [
        { ...relation("1", "2"), parent_group_id: "7", child_group_id: "7" },
        { ...relation("1", "3"), id: "2", parent_group_id: "7", child_group_id: "8" },
        { ...relation("1", "4"), id: "3", parent_group_id: "8", child_group_id: "7" },
        { ...relation("6", "6"), id: "self", parent_group_id: "7", child_group_id: "9" },
        {
          ...relation("1", "5"),
          id: "4",
          parent_group_id: "7",
          child_group_id: "9",
          state: "retracted",
        },
      ]),
    ).toEqual([{ groupId: "8" }]);
  });

  it("keeps actionable candidates while hiding self and superseded candidates", () => {
    const candidate = (
      overrides: Partial<ThreadGroupUnresolvedView> = {},
    ): ThreadGroupUnresolvedView => ({
      id: "candidate-1",
      subject_thread_id: "1",
      subject_source: "codex",
      subject_user_id: 2,
      candidate_group_id: "8",
      candidate_parent_thread_id: "2",
      state: "pending",
      subject_identity_scope_known: true,
      subject_identity_scope_value: "scope",
      subject_native_id: "native",
      selected_observation_id: "observation-1",
      ...overrides,
    });
    expect(
      inferredCandidates([
        candidate(),
        candidate({ id: "detected", state: "candidate", selected_observation_id: null }),
        candidate({ id: "self", candidate_parent_thread_id: "1" }),
        candidate({ id: "ignored", state: "superseded" }),
      ]),
    ).toEqual([
      candidate(),
      candidate({ id: "detected", state: "candidate", selected_observation_id: null }),
    ]);
  });

  it("projects an inferred tree edge only between distinct live group members", () => {
    const candidate = (
      id: string,
      subjectThreadId: string | null,
      parentThreadId: string | null,
    ): ThreadGroupUnresolvedView => ({
      id,
      subject_thread_id: subjectThreadId,
      subject_source: "codex",
      subject_user_id: 2,
      candidate_group_id: "7",
      candidate_parent_thread_id: parentThreadId,
      state: "pending",
      subject_identity_scope_known: true,
      subject_identity_scope_value: "scope",
      subject_native_id: `native-${id}`,
      selected_observation_id: null,
    });

    expect(
      inferredTreeRelations(
        [member("1"), member("2"), member("3", "deleted")],
        [
          candidate("valid", "2", "1"),
          candidate("missing-parent", "1", null),
          candidate("external-parent", "1", "4"),
          candidate("deleted-child", "3", "1"),
          candidate("self", "1", "1"),
        ],
      ),
    ).toEqual([{ parentThreadId: "1", childThreadId: "2" }]);
  });

  it("projects confirmed and inferred tree edges with confirmed edges taking precedence", () => {
    const candidate = (
      subjectThreadId: string,
      parentThreadId: string,
    ): ThreadGroupUnresolvedView => ({
      id: `${subjectThreadId}-${parentThreadId}`,
      subject_thread_id: subjectThreadId,
      subject_source: "codex",
      subject_user_id: 2,
      candidate_group_id: "7",
      candidate_parent_thread_id: parentThreadId,
      state: "candidate",
      subject_identity_scope_known: true,
      subject_identity_scope_value: "scope",
      subject_native_id: `native-${subjectThreadId}`,
      selected_observation_id: null,
    });

    const projection = threadGroupTreeProjection(
      [member("3"), member("2"), member("1")],
      [relation("1", "2")],
      [candidate("2", "3"), candidate("3", "1")],
    );

    expect(projection.members.map((item) => item.thread_id)).toEqual(["1", "2", "3"]);
    expect(projection.parentByChild.get("2")).toBe("1");
    expect(projection.parentByChild.get("3")).toBe("1");
    expect(projection.relationByChild.get("2")?.id).toBe("1");
    expect(projection.inferredRelationByChild.has("2")).toBe(false);
    expect(projection.inferredRelationByChild.get("3")).toEqual({
      parentThreadId: "1",
      childThreadId: "3",
    });

    const crossGroupProjection = threadGroupTreeProjection(
      [member("2"), member("1")],
      [{ ...relation("1", "2"), parent_group_id: "7", child_group_id: "8" }],
      [candidate("2", "1")],
    );
    expect(crossGroupProjection.members.map((item) => item.thread_id)).toEqual(["1", "2"]);
    expect(crossGroupProjection.relationByChild.has("2")).toBe(false);
    expect(crossGroupProjection.inferredRelationByChild.get("2")).toEqual({
      parentThreadId: "1",
      childThreadId: "2",
    });
  });
});

describe("fetchThreadGroupSummaries", () => {
  const summary = (id: string) => ({
    memory_id: id,
    external_id: `thread-group-summary:${id}`,
    content_json: "{}",
    metadata: null,
    updated_at_ms: 0,
  });

  it("issues one request per owner and maps every group to its summary", async () => {
    const calls: Array<[string[], number]> = [];
    const fetchBatch = async (ids: string[], owner?: number) => {
      calls.push([ids, owner as number]);
      return ids.map((group_id) => ({
        group_id,
        summary: group_id === "8" ? null : summary(group_id),
      }));
    };
    const result = await fetchThreadGroupSummaries(
      [
        group({ id: "7", user_id: 1 }),
        group({ id: "8", user_id: 1 }),
        group({ id: "9", user_id: 2 }),
      ],
      fetchBatch,
    );
    expect(calls).toEqual([
      [["7", "8"], 1],
      [["9"], 2],
    ]);
    expect(result.get("7")?.memory_id).toBe("7");
    expect(result.get("8")).toBeNull();
    expect(result.get("9")?.memory_id).toBe("9");
  });

  it("does not call the backend for an empty page", async () => {
    let called = false;
    const result = await fetchThreadGroupSummaries([], async () => {
      called = true;
      return [];
    });
    expect(called).toBe(false);
    expect(result.size).toBe(0);
  });
});
