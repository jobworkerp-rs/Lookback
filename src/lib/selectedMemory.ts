import { getSelectedMemoryLimits as getSelectedMemoryLimitsFromApi } from "@/api";
import type {
  SelectableMemoryKind,
  SelectedMemoryLimits,
  SelectedMemorySnapshot,
} from "@/types/api";

export const DEFAULT_SELECTED_MEMORY_LIMITS: SelectedMemoryLimits = {
  max_selected: 10,
  max_direct_ids_per_memory: 100,
  max_direct_ids_total: 500,
  max_tool_ids_per_call: 10,
  default_max_items_per_thread: 20,
  max_items_per_thread: 100,
  default_retrieval_hops: 4,
  max_retrieval_hops: 8,
  min_selected_tokens: 256,
};

/** Retrieve the backend-owned limits so UI validation stays aligned with the
 * command that ultimately accepts selected-memory snapshots. */
export async function getSelectedMemoryLimits(): Promise<SelectedMemoryLimits> {
  return getSelectedMemoryLimitsFromApi();
}

const SELECTABLE_MEMORY_KINDS: readonly SelectableMemoryKind[] = [
  "per-thread",
  "daily",
  "weekly",
  "monthly",
  "reflection",
];
const MAX_I64 = 9_223_372_036_854_775_807n;

function validateId(id: string, field: string): void {
  if (!/^[1-9][0-9]*$/.test(id)) throw new Error(`${field} must be a decimal string`);
  if (BigInt(id) > MAX_I64) throw new Error(`${field} is outside i64 range`);
}

export function validateSelectedMemories(
  selected: SelectedMemorySnapshot[] | undefined,
  limits: SelectedMemoryLimits = DEFAULT_SELECTED_MEMORY_LIMITS,
): SelectedMemorySnapshot[] {
  if (!selected) return [];
  if (selected.length > limits.max_selected) throw new Error("too many selected memories");
  const ids = new Set<string>();
  for (const [index, item] of selected.entries()) {
    validateId(item.memory_id, `selected_memories[${index}].memory_id`);
    if (ids.has(item.memory_id)) throw new Error("duplicate selected memory_id");
    ids.add(item.memory_id);
    if (!SELECTABLE_MEMORY_KINDS.includes(item.kind))
      throw new Error("invalid selected memory kind");
    if (!item.content.trim()) throw new Error("selected memory content is empty");
    if (!Number.isFinite(item.captured_at_ms)) throw new Error("invalid captured_at_ms");
    for (const id of item.source_memory_ids ?? []) validateId(id, "source_memory_ids");
    for (const id of item.source_thread_ids ?? []) validateId(id, "source_thread_ids");
  }
  return selected;
}

export function normalizeSelectedMemories(
  selected: SelectedMemorySnapshot[],
  limits: SelectedMemoryLimits = DEFAULT_SELECTED_MEMORY_LIMITS,
): SelectedMemorySnapshot[] {
  const seenMemoryIds = new Set<string>();
  const seenSourceMemoryIds = new Set<string>();
  const seenSourceThreadIds = new Set<string>();
  let total = 0;
  return selected.slice(0, limits.max_selected).filter((item) => {
    if (seenMemoryIds.has(item.memory_id)) return false;
    seenMemoryIds.add(item.memory_id);
    const originalMemoryIds = [...new Set(item.source_memory_ids ?? [])];
    const originalThreadIds = [...new Set(item.source_thread_ids ?? [])];
    const uniqueMemoryIds = originalMemoryIds.filter((id) => !seenSourceMemoryIds.has(id));
    const uniqueThreadIds = originalThreadIds.filter((id) => !seenSourceThreadIds.has(id));
    const memoryIds = uniqueMemoryIds.slice(0, limits.max_direct_ids_per_memory);
    const threadIds = uniqueThreadIds.slice(
      0,
      Math.max(0, limits.max_direct_ids_per_memory - memoryIds.length),
    );
    const remaining = Math.max(0, limits.max_direct_ids_total - total);
    const cappedMemoryIds = memoryIds.slice(0, remaining);
    const cappedThreadIds = threadIds.slice(0, Math.max(0, remaining - cappedMemoryIds.length));
    for (const id of cappedMemoryIds) seenSourceMemoryIds.add(id);
    for (const id of cappedThreadIds) seenSourceThreadIds.add(id);
    total += cappedMemoryIds.length + cappedThreadIds.length;
    item.source_memory_ids = cappedMemoryIds;
    item.source_thread_ids = cappedThreadIds;
    item.source_ids_truncated =
      cappedMemoryIds.length !== uniqueMemoryIds.length ||
      cappedThreadIds.length !== uniqueThreadIds.length ||
      uniqueMemoryIds.length !== originalMemoryIds.length ||
      uniqueThreadIds.length !== originalThreadIds.length;
    return true;
  });
}

interface SourceProjection {
  content: string;
  allowedMemoryIds: string[];
  allowedThreadIds: string[];
}

const SOURCE_KEYS = new Set(["source_memory_ids", "source_thread_ids"]);

function collectIds(value: unknown, memoryIds: Set<string>, threadIds: Set<string>): unknown {
  if (Array.isArray(value)) return value.filter((v) => typeof v === "string");
  if (!value || typeof value !== "object") return value;
  const out: Record<string, unknown> = {};
  for (const [key, child] of Object.entries(value)) {
    if (SOURCE_KEYS.has(key)) {
      const values = Array.isArray(child)
        ? child.filter((v): v is string => typeof v === "string")
        : [];
      const target = key === "source_memory_ids" ? memoryIds : threadIds;
      for (const id of values) {
        try {
          validateId(id, key);
          target.add(id);
        } catch {
          // Malformed structured references are omitted from callable metadata.
        }
      }
      continue;
    }
    out[key] = collectIds(child, memoryIds, threadIds);
  }
  return out;
}

export function projectSelectedMemoryContent(content: string): SourceProjection {
  try {
    const parsed: unknown = JSON.parse(content);
    const memoryIds = new Set<string>();
    const threadIds = new Set<string>();
    const projected = collectIds(parsed, memoryIds, threadIds);
    return {
      content: JSON.stringify(projected),
      allowedMemoryIds: [...memoryIds],
      allowedThreadIds: [...threadIds],
    };
  } catch {
    return { content, allowedMemoryIds: [], allowedThreadIds: [] };
  }
}
