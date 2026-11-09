import type {
  ThreadGroupListItem,
  ThreadGroupMemberView,
  ThreadGroupSummaryEntryView,
  ThreadGroupSummaryView,
  ThreadGroupUnresolvedView,
  ThreadRelationView,
} from "@/types/api";
import { parseThreadDescription } from "./threadDescription";

export type ThreadGroupSummaryState = "latest" | "stale" | "absent" | "generating" | "failed";

/**
 * Summary lookups are owner-scoped, so a page is fetched with one batched
 * request per distinct owner (normally exactly one) instead of one per card.
 */
export async function fetchThreadGroupSummaries(
  groups: ThreadGroupListItem[],
  fetchBatch: (
    groupIds: string[],
    groupOwnerUserId?: number,
  ) => Promise<ThreadGroupSummaryEntryView[]>,
): Promise<Map<string, ThreadGroupSummaryView | null>> {
  const idsByOwner = new Map<number, string[]>();
  for (const group of groups) {
    const ids = idsByOwner.get(group.user_id) ?? [];
    ids.push(group.id);
    idsByOwner.set(group.user_id, ids);
  }
  const batches = await Promise.all([...idsByOwner].map(([owner, ids]) => fetchBatch(ids, owner)));
  return new Map(batches.flat().map((entry) => [entry.group_id, entry.summary]));
}

export function groupDisplayTitle(group: ThreadGroupListItem): string {
  const title = group.title?.trim();
  if (title) return title;
  const rootDescription = group.root_display?.description?.trim();
  if (rootDescription) return parseThreadDescription(rootDescription, group.display_name).title;
  return group.display_name;
}

export function liveThreadId(member: ThreadGroupMemberView): string | null {
  return member.state === "active" && member.thread_id != null ? member.thread_id : null;
}

export function orderGroupMembers(
  members: ThreadGroupMemberView[],
  relations: Pick<ThreadRelationView, "parent_thread_id" | "child_thread_id" | "state">[],
): ThreadGroupMemberView[] {
  const liveMembers = new Map(
    members
      .filter((member) => liveThreadId(member) != null)
      .map((member) => [liveThreadId(member) as string, member]),
  );
  const children = new Map<string, string[]>();
  const childIds = new Set<string>();
  for (const relation of relations) {
    if (
      relation.state !== "active" ||
      relation.parent_thread_id == null ||
      relation.child_thread_id == null
    ) {
      continue;
    }
    if (!liveMembers.has(relation.parent_thread_id) || !liveMembers.has(relation.child_thread_id)) {
      continue;
    }
    children.set(relation.parent_thread_id, [
      ...(children.get(relation.parent_thread_id) ?? []),
      relation.child_thread_id,
    ]);
    childIds.add(relation.child_thread_id);
  }

  const orderedIds: string[] = [];
  const visited = new Set<string>();
  const visit = (threadId: string) => {
    if (visited.has(threadId)) return;
    visited.add(threadId);
    orderedIds.push(threadId);
    for (const childId of children.get(threadId) ?? []) visit(childId);
  };
  for (const threadId of liveMembers.keys()) {
    if (!childIds.has(threadId)) visit(threadId);
  }
  for (const threadId of liveMembers.keys()) visit(threadId);

  const ordered = orderedIds.map((threadId) => liveMembers.get(threadId) as ThreadGroupMemberView);
  return [...ordered, ...members.filter((member) => liveThreadId(member) == null)];
}

export function relatedGroups(
  currentGroupId: string,
  relations: ThreadRelationView[],
): { groupId: string }[] {
  const groupIds = new Set<string>();
  for (const relation of relations) {
    if (relation.state !== "active") continue;
    if (
      relation.parent_thread_id != null &&
      relation.parent_thread_id === relation.child_thread_id
    ) {
      continue;
    }
    for (const groupId of [relation.parent_group_id, relation.child_group_id]) {
      if (groupId && groupId !== currentGroupId) groupIds.add(groupId);
    }
  }
  return [...groupIds].sort().map((groupId) => ({ groupId }));
}

export function inferredCandidates(
  candidates: ThreadGroupUnresolvedView[],
): ThreadGroupUnresolvedView[] {
  return candidates.filter(
    (candidate) =>
      ["pending", "candidate", "ambiguous", "conflict"].includes(candidate.state) &&
      candidate.subject_thread_id !== candidate.candidate_parent_thread_id,
  );
}

export function inferredTreeRelations(
  members: ThreadGroupMemberView[],
  candidates: ThreadGroupUnresolvedView[],
): { parentThreadId: string; childThreadId: string }[] {
  const liveMemberIds = new Set(
    members.map(liveThreadId).filter((threadId): threadId is string => threadId != null),
  );
  return inferredCandidates(candidates).flatMap((candidate) => {
    const parentThreadId = candidate.candidate_parent_thread_id;
    const childThreadId = candidate.subject_thread_id;
    if (
      parentThreadId == null ||
      childThreadId == null ||
      !liveMemberIds.has(parentThreadId) ||
      !liveMemberIds.has(childThreadId)
    ) {
      return [];
    }
    return [{ parentThreadId, childThreadId }];
  });
}

type InferredTreeRelation = { parentThreadId: string; childThreadId: string };

export interface ThreadGroupTreeProjection {
  members: ThreadGroupMemberView[];
  parentByChild: Map<string, string>;
  relationByChild: Map<string, ThreadRelationView>;
  inferredRelationByChild: Map<string, InferredTreeRelation>;
}

export function threadGroupTreeProjection(
  members: ThreadGroupMemberView[],
  relations: ThreadRelationView[],
  candidates: ThreadGroupUnresolvedView[],
): ThreadGroupTreeProjection {
  const treeRelations = relations.filter(
    (relation) =>
      relation.state === "active" &&
      relation.parent_thread_id != null &&
      relation.child_thread_id != null &&
      relation.parent_thread_id !== relation.child_thread_id &&
      (relation.parent_group_id == null ||
        relation.child_group_id == null ||
        relation.parent_group_id === relation.child_group_id),
  );
  const relationByChild = new Map(
    treeRelations.map((relation) => [relation.child_thread_id as string, relation]),
  );
  const inferredRelationByChild = new Map(
    inferredTreeRelations(members, candidates)
      .filter(({ childThreadId }) => !relationByChild.has(childThreadId))
      .map((relation) => [relation.childThreadId, relation]),
  );
  const effectiveInferredRelations = [...inferredRelationByChild.values()];
  const parentByChild = new Map<string, string>([
    ...effectiveInferredRelations.map(
      ({ childThreadId, parentThreadId }) => [childThreadId, parentThreadId] as const,
    ),
    ...treeRelations.map(
      (relation) =>
        [relation.child_thread_id as string, relation.parent_thread_id as string] as const,
    ),
  ]);
  const orderingRelations = [
    ...treeRelations,
    ...effectiveInferredRelations.map(({ parentThreadId, childThreadId }) => ({
      parent_thread_id: parentThreadId,
      child_thread_id: childThreadId,
      state: "active",
    })),
  ];
  return {
    members: orderGroupMembers(members, orderingRelations),
    parentByChild,
    relationByChild,
    inferredRelationByChild,
  };
}

export function isThreadGroupUnsupportedError(error: unknown): boolean {
  const message = error instanceof Error ? error.message : typeof error === "string" ? error : "";
  return (
    message.includes("スレッドグループ表示に未対応") ||
    /thread.?group.*unsupported/i.test(message) ||
    /unknown service.*ThreadGroupService/i.test(message)
  );
}

export function isThreadGroupSnapshotChangedError(error: unknown): boolean {
  const message = error instanceof Error ? error.message : typeof error === "string" ? error : "";
  return /snapshot[_ ]changed/i.test(message);
}

/**
 * Resolve the user-visible group summary state in one place. An in-flight
 * generation hides both an older body and its prior failure; a durable failure
 * hides digest comparison so the retry action is not mistaken for freshness.
 */
export function threadGroupSummaryState({
  group,
  summary,
  generating,
  failed,
}: {
  group: ThreadGroupListItem;
  summary: ThreadGroupSummaryView | null | undefined;
  generating: boolean;
  failed: boolean;
}): ThreadGroupSummaryState {
  if (generating) return "generating";
  if (failed) return "failed";
  if (!summary) return "absent";
  const generatedDigest =
    summary.membership_snapshot_digest ?? readSummaryDigest(summary.metadata ?? undefined);
  return generatedDigest === group.membership_snapshot_digest ? "latest" : "stale";
}

function readSummaryDigest(metadata: string | undefined): string | null {
  if (!metadata) return null;
  try {
    const parsed: unknown = JSON.parse(metadata);
    if (typeof parsed !== "object" || parsed == null) return null;
    const digest = (parsed as { membership_snapshot_digest?: unknown }).membership_snapshot_digest;
    return typeof digest === "string" && digest.length > 0 ? digest : null;
  } catch {
    return null;
  }
}
