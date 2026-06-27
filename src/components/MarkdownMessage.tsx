import { memo, useMemo } from "react";
import { useTranslation } from "react-i18next";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { useLocaleTag } from "@/hooks/useLocaleTag";
import { isCodexInjectedUserMessage } from "@/lib/codexMemoryVisibility";
import { formatDateTime, formatNumber } from "@/lib/localeFormat";
import { claudeCodeDisplayHints, isTagOnlyMessage } from "@/lib/systemMessage";

/// Single place the remark plugin list lives, so every markdown surface
/// (chat messages, thread summaries, reflection cards) renders with the same
/// feature set. Memoized on the source string so a list re-render (e.g. the
/// reflection list, where each card renders many MarkdownBody instances) only
/// re-parses items whose text actually changed.
export const MarkdownBody = memo(function MarkdownBody({ children }: { children: string }) {
  return <ReactMarkdown remarkPlugins={[remarkGfm]}>{children}</ReactMarkdown>;
});

// memories MessageRole enum (proto llm_memory/data/common.proto). Single source
// for the magic role numbers so the tables below stay in sync.
export const MessageRole = {
  Unspecified: 0,
  User: 1,
  Assistant: 2,
  System: 3,
  Tool: 4,
  Meta: 5,
  Reflection: 6,
} as const;

export const MESSAGE_ROLE_LABEL: Record<number, string> = {
  [MessageRole.Unspecified]: "Unspecified",
  [MessageRole.User]: "User",
  [MessageRole.Assistant]: "Assistant",
  [MessageRole.System]: "System",
  [MessageRole.Tool]: "Tool",
  [MessageRole.Meta]: "Meta",
  [MessageRole.Reflection]: "Reflection",
};

const ROLE_TONE: Record<number, string> = {
  [MessageRole.User]: "user",
  [MessageRole.Assistant]: "assistant",
  [MessageRole.System]: "system",
  [MessageRole.Tool]: "tool",
  [MessageRole.Meta]: "system",
  [MessageRole.Reflection]: "system",
};

// Meta (codex 'developer' / claude-code 'system' / token_count / task_complete)
// and Reflection are always machine-inserted scaffolding, never conversation.
// Fold them by content shape regardless (tags, JSON, or plain system prompts).
const FOLD_ROLES = new Set<number>([MessageRole.Meta, MessageRole.Reflection]);
// 4 KiB content is roughly where MCP tool outputs (e.g. file dumps,
// `tree -L 3`) start exhausting the React renderer's vDOM diff budget.
// We keep the first KiB + last 0.5 KiB so both the call site and the
// trailing result remain visible.
const TOOL_TRUNCATE_THRESHOLD = 4096;
const TOOL_HEAD_KEEP = 1024;
const TOOL_TAIL_KEEP = 512;

export interface MarkdownMessageProps {
  role: number;
  createdAtMs: number;
  content: string;
  metadata?: string | null;
  /** Memory id, used as the scroll anchor when opened from a search hit. */
  memoryId?: string;
  /** Briefly flashes the message background to mark the search hit. */
  highlight?: boolean;
}

/** DOM id of a memory row, so ThreadDetail can scroll to a search hit. */
export function memoryDomId(memoryId: string): string {
  return `memory-${memoryId}`;
}

// Memoized: ThreadDetail re-renders the whole list on every page load; without
// this every prior row re-parses its markdown. Props are primitives so the
// default shallow compare is sufficient.
export const MarkdownMessage = memo(function MarkdownMessage({
  role,
  createdAtMs,
  content,
  metadata,
  memoryId,
  highlight,
}: MarkdownMessageProps) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const tone = ROLE_TONE[role] ?? "other";
  // Computed once per (role, content, metadata) tuple so each row's metadata
  // JSON is parsed at most once across the fold/command/markdown branches —
  // ThreadDetail re-renders on highlight changes and scroll, so a fresh parse
  // per render would scale at O(rows × branches) for no reason.
  const mode = useMemo(
    () => resolveDisplayMode(role, content, metadata),
    [role, content, metadata],
  );
  const wrapperProps = {
    id: memoryId ? memoryDomId(memoryId) : undefined,
    className: `message message-${tone}${highlight ? " message-hit" : ""}`,
  };
  const header = (
    <div className="message-head">
      <span className={`role-badge role-${tone}`}>{MESSAGE_ROLE_LABEL[role] ?? "?"}</span>
      <span className="message-time">{formatDateTime(createdAtMs, locale)}</span>
    </div>
  );
  if (mode.kind === "tool") {
    const { display, truncated } = truncateForTool(content, locale);
    return (
      <div {...wrapperProps}>
        {header}
        <details className="tool-details">
          <summary>
            {truncated
              ? t("markdown.toolOutputTruncated", { count: formatNumber(content.length, locale) })
              : t("markdown.toolOutput", { count: formatNumber(content.length, locale) })}
          </summary>
          <pre className="tool-body">{display}</pre>
        </details>
      </div>
    );
  }
  if (mode.kind === "command") {
    return (
      <div {...wrapperProps}>
        {header}
        <div className="message-body">
          <p>
            <code>{mode.commandName}</code>
          </p>
        </div>
      </div>
    );
  }
  // System-inserted scaffolding is folded by default so it doesn't clutter the
  // thread: either a Meta/Reflection role, or content that is HTML tags only
  // (e.g. `<command-name>/clear</command-name>` saved under a normal role).
  // Shown raw in a <pre> so tags/JSON read verbatim and `/` `#` aren't
  // misparsed as markdown.
  if (mode.kind === "fold") {
    return (
      <div {...wrapperProps}>
        {header}
        <details className="system-fold">
          <summary>{t("markdown.systemMessage")}</summary>
          <pre className="system-fold-body">{content}</pre>
        </details>
      </div>
    );
  }
  return (
    <div {...wrapperProps}>
      {header}
      <div className="message-body">
        <MarkdownBody>{content}</MarkdownBody>
      </div>
    </div>
  );
});

type DisplayMode =
  | { kind: "tool" }
  | { kind: "command"; commandName: string }
  | { kind: "fold" }
  | { kind: "markdown" };

function resolveDisplayMode(
  role: number,
  content: string,
  metadata: string | null | undefined,
): DisplayMode {
  if (role === MessageRole.Tool) return { kind: "tool" };
  const hints = claudeCodeDisplayHints(content, metadata);
  if (hints.commandName) return { kind: "command", commandName: hints.commandName };
  if (
    FOLD_ROLES.has(role) ||
    hints.collapsed ||
    isTagOnlyMessage(content) ||
    isCodexInjectedUserMessage(content, metadata)
  ) {
    return { kind: "fold" };
  }
  return { kind: "markdown" };
}

/**
 * Cap tool output at ~1.5 KiB shown for very large payloads so the
 * `<details>` doesn't expand to multi-megabyte JSON blobs. Returns the
 * original content (and `truncated = false`) when below threshold.
 */
export function truncateForTool(
  content: string,
  locale?: string,
): { display: string; truncated: boolean } {
  if (content.length <= TOOL_TRUNCATE_THRESHOLD) {
    return { display: content, truncated: false };
  }
  const head = content.slice(0, TOOL_HEAD_KEEP);
  const tail = content.slice(-TOOL_TAIL_KEEP);
  const omittedChars = content.length - TOOL_HEAD_KEEP - TOOL_TAIL_KEEP;
  return {
    display: `${head}\n\n--- [truncated ${formatNumber(omittedChars, locale)} chars] ---\n\n${tail}`,
    truncated: true,
  };
}
