export type ThreadReference = { kind: "both" | "thread" | "memory"; id: string } | null;

/**
 * Recognize an exact numeric identifier in the Threads browse filter without
 * coercing it through JavaScript's unsafe Number representation.
 */
export function parseThreadReference(value: string): ThreadReference {
  const match = /^(?:(thread|memory):)?(\d+)$/i.exec(value.trim());
  if (!match) return null;
  const [, prefix, id] = match;
  if (!id) return null;
  return {
    kind: (prefix?.toLowerCase() as "thread" | "memory" | undefined) ?? "both",
    id,
  };
}
