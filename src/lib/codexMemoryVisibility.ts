import type { MemoryRow } from "@/types/api";

const USER_ROLE = 1;
const ASSISTANT_ROLE = 2;
const NEARBY_WINDOW = 3;
const PRIMARY_PAYLOAD_TYPE = "message";
const ASSISTANT_EVENT_PAYLOAD_TYPE = "agent_message";
const USER_EVENT_PAYLOAD_TYPE = "user_message";

interface CodexMetadata {
  source?: unknown;
  kind?: unknown;
  payload_type?: unknown;
  block_type?: unknown;
}

interface VisibilityOptions {
  alwaysIncludeIds?: readonly string[];
}

export function visibleConversationMemories(
  memories: readonly MemoryRow[],
  options: VisibilityOptions = {},
): MemoryRow[] {
  const alwaysIncludeIds = new Set(options.alwaysIncludeIds ?? []);
  return memories.filter((memory, index) => {
    if (alwaysIncludeIds.has(memory.id)) return true;
    if (isAssistantEventShadow(memory)) {
      return !hasNearbyMatch(memories, memory, index, isPrimaryCodexMessage);
    }
    if (isUserResponseItemShadow(memory)) {
      return !hasNearbyMatch(memories, memory, index, isUserEventMessage);
    }
    return true;
  });
}

// Scans the ±NEARBY_WINDOW neighbourhood for a same-role/same-content row that
// satisfies `matches` — the canonical twin that lets us drop `shadow`.
function hasNearbyMatch(
  memories: readonly MemoryRow[],
  shadow: MemoryRow,
  index: number,
  matches: (candidate: MemoryRow) => boolean,
): boolean {
  const start = Math.max(0, index - NEARBY_WINDOW);
  const end = Math.min(memories.length - 1, index + NEARBY_WINDOW);
  for (let i = start; i <= end; i += 1) {
    if (i === index) continue;
    const candidate = memories[i];
    if (
      candidate &&
      candidate.role === shadow.role &&
      candidate.content === shadow.content &&
      matches(candidate)
    ) {
      return true;
    }
  }
  return false;
}

function isPrimaryCodexMessage(memory: MemoryRow): boolean {
  const metadata = parseMetadata(memory.metadata);
  return (
    isCodexConversationMetadata(metadata) &&
    metadata.payload_type === PRIMARY_PAYLOAD_TYPE &&
    roleMatchesKind(memory.role, metadata.kind)
  );
}

function isAssistantEventShadow(memory: MemoryRow): boolean {
  const metadata = parseMetadata(memory.metadata);
  return (
    isCodexConversationMetadata(metadata) &&
    metadata.kind === "assistant" &&
    metadata.payload_type === ASSISTANT_EVENT_PAYLOAD_TYPE &&
    roleMatchesKind(memory.role, metadata.kind)
  );
}

function isUserResponseItemShadow(memory: MemoryRow): boolean {
  const metadata = parseMetadata(memory.metadata);
  return (
    isCodexConversationMetadata(metadata) &&
    metadata.kind === "user" &&
    metadata.payload_type === PRIMARY_PAYLOAD_TYPE &&
    roleMatchesKind(memory.role, metadata.kind)
  );
}

function isUserEventMessage(memory: MemoryRow): boolean {
  const metadata = parseMetadata(memory.metadata);
  return (
    isCodexConversationMetadata(metadata) &&
    metadata.kind === "user" &&
    metadata.payload_type === USER_EVENT_PAYLOAD_TYPE &&
    roleMatchesKind(memory.role, metadata.kind)
  );
}

// Tag-shaped prefixes Codex injects into the *user* role as `response_item`
// messages (environment/permissions context, abort markers). Real user input
// almost never starts with one of these tags, so a bare prefix match is safe.
const CODEX_INJECTED_TAG_PREFIXES = [
  "<environment_context>",
  "<permissions instructions>",
  "<turn_aborted>",
] as const;

// Markdown-heading-shaped injection (AGENTS.md / CLAUDE.md scaffolding). The
// heading line alone (`# AGENTS.md instructions ...`) is shaped exactly like
// something a user might legitimately type as the opening of a question, so
// we additionally require Codex's enclosing `<INSTRUCTIONS>` block on a later
// line — that pairing is the unambiguous injection marker.
const CODEX_INJECTED_HEADING_PREFIXES = ["# AGENTS.md instructions", "# CLAUDE.md instructions"];
const INSTRUCTIONS_BLOCK_MARKER = "<INSTRUCTIONS>";

/**
 * True when a user-role memory is Codex-injected scaffolding (matched by both
 * the `response_item` metadata shape and a known injected-content prefix)
 * rather than a message the user actually typed.
 */
export function isCodexInjectedUserMessage(
  content: string,
  metadata: string | null | undefined,
): boolean {
  const parsed = parseMetadata(metadata);
  if (
    !isCodexConversationMetadata(parsed) ||
    parsed.kind !== "user" ||
    parsed.payload_type !== PRIMARY_PAYLOAD_TYPE ||
    parsed.block_type !== "input_text"
  ) {
    return false;
  }

  const text = content.trimStart();
  if (CODEX_INJECTED_TAG_PREFIXES.some((prefix) => text.startsWith(prefix))) return true;
  // The heading + <INSTRUCTIONS> block must co-occur, otherwise a user
  // writing `# AGENTS.md instructions are documented at …` as a question
  // would be silently folded as scaffolding.
  return (
    CODEX_INJECTED_HEADING_PREFIXES.some((prefix) => text.startsWith(prefix)) &&
    text.includes(INSTRUCTIONS_BLOCK_MARKER)
  );
}

function isCodexConversationMetadata(
  metadata: CodexMetadata | null,
): metadata is Required<Pick<CodexMetadata, "source" | "kind">> & CodexMetadata {
  return (
    metadata?.source === "codex" && (metadata.kind === "user" || metadata.kind === "assistant")
  );
}

function roleMatchesKind(role: number, kind: unknown): boolean {
  return (
    (kind === "user" && role === USER_ROLE) || (kind === "assistant" && role === ASSISTANT_ROLE)
  );
}

function parseMetadata(metadata: string | null | undefined): CodexMetadata | null {
  return parseMessageMetadata<CodexMetadata>(metadata);
}

/**
 * Shared JSON-blob → object parse for memory.metadata payloads. Returns null
 * for missing / malformed input or non-object roots; callers downcast to their
 * provider-specific shape and narrow by `source`.
 */
export function parseMessageMetadata<T>(metadata: string | null | undefined): T | null {
  if (!metadata) return null;
  try {
    const parsed: unknown = JSON.parse(metadata);
    return parsed && typeof parsed === "object" ? (parsed as T) : null;
  } catch {
    return null;
  }
}
