import { parseMessageMetadata } from "@/lib/codexMemoryVisibility";

// One opening / closing / self-closing tag. The name is restricted to an ASCII
// letter start so bare inequalities like "< 5" or "a < b" are not mistaken for
// tags. Known limitation: a ">" inside an attribute value (e.g. <a title="a>b">)
// terminates the match early — acceptable since system-inserted tags carry no
// such attributes.
const TAG = /^<(\/)?[a-zA-Z][a-zA-Z0-9-]*(?:\s[^>]*?)?(\/)?>/;
const COMMAND_NAME_TAG = /<command-name>([\s\S]*?)<\/command-name>/i;
const COMMAND_MESSAGE_TAG = /<command-message>([\s\S]*?)<\/command-message>/i;

interface ClaudeCodeMetadata {
  source?: unknown;
  kind?: unknown;
  entry_type?: unknown;
  block_type?: unknown;
  subtype?: unknown;
  claude_code?: {
    is_meta?: unknown;
    user_type?: unknown;
  };
}

export interface ClaudeCodeDisplayHints {
  collapsed: boolean;
  commandName: string | null;
}

/**
 * True when `content` is made up solely of HTML tags (and whitespace between
 * them), with no real body text at the top level — i.e. a system-inserted
 * message such as `<command-name>/clear</command-name>` or a single
 * `<local-command-caveat>…</local-command-caveat>` wrapper. Text *inside* tags
 * is ignored; only the top level (outside any element) is inspected.
 *
 * Errs toward not folding: plain text, text outside tags, and broken/unclosed
 * tags all return false, so a normal message is never hidden by mistake.
 */
export function isTagOnlyMessage(content: string): boolean {
  const text = content.trim();
  if (text === "") return false;

  let depth = 0;
  let sawTag = false;
  let i = 0;
  while (i < text.length) {
    if (text[i] === "<") {
      const m = TAG.exec(text.slice(i));
      if (m) {
        sawTag = true;
        const isClose = !!m[1];
        const isSelfClose = !!m[2];
        if (isClose) {
          depth = Math.max(0, depth - 1);
        } else if (!isSelfClose) {
          depth += 1;
        }
        i += m[0].length;
        continue;
      }
      // A "<" that doesn't start a valid tag. At the top level that's body
      // text; inside an element it's just content and is ignored.
      if (depth === 0) return false;
      i += 1;
      continue;
    }
    if (depth === 0 && !/\s/.test(text[i] as string)) return false;
    i += 1;
  }

  return sawTag && depth === 0;
}

/**
 * Display hints derived from the server-side Claude Code metadata contract.
 * Falls back to normal display when the contract is absent so legacy imports
 * do not hide real user text through content guessing.
 */
export function claudeCodeDisplayHints(
  content: string,
  metadata: string | null | undefined,
): ClaudeCodeDisplayHints {
  const parsed = parseClaudeCodeMetadata(metadata);
  if (!parsed) return { collapsed: false, commandName: null };

  if (parsed.claude_code?.is_meta === true && parsed.claude_code.user_type === "external") {
    return { collapsed: true, commandName: null };
  }

  if (
    parsed.kind === "attachment" ||
    parsed.kind === "system" ||
    typeof parsed.subtype === "string"
  ) {
    return { collapsed: true, commandName: null };
  }

  const commandName = isClaudeSlashCommandInvocation(parsed)
    ? claudeSlashCommandName(content)
    : null;
  return { collapsed: false, commandName };
}

export function claudeSlashCommandName(content: string): string | null {
  if (!isTagOnlyMessage(content)) return null;
  const commandName = tagText(content, COMMAND_NAME_TAG);
  if (commandName) return commandName;
  // <command-message> body never carries a leading slash in observed claude_code
  // logs, so prefix unconditionally to surface the same "/foo" display shape.
  const commandMessage = tagText(content, COMMAND_MESSAGE_TAG);
  return commandMessage ? `/${commandMessage}` : null;
}

function isClaudeSlashCommandInvocation(metadata: ClaudeCodeMetadata): boolean {
  return (
    metadata.kind === "user" &&
    metadata.entry_type === "user" &&
    metadata.block_type === "text" &&
    metadata.claude_code != null &&
    metadata.claude_code.is_meta !== true
  );
}

function parseClaudeCodeMetadata(metadata: string | null | undefined): ClaudeCodeMetadata | null {
  const meta = parseMessageMetadata<ClaudeCodeMetadata>(metadata);
  return meta?.source === "claude_code" ? meta : null;
}

function tagText(content: string, pattern: RegExp): string | null {
  const text = pattern.exec(content)?.[1]?.trim();
  return text ? decodeBasicEntities(text) : null;
}

function decodeBasicEntities(text: string): string {
  return text
    .replaceAll("&lt;", "<")
    .replaceAll("&gt;", ">")
    .replaceAll("&amp;", "&")
    .replaceAll("&quot;", '"')
    .replaceAll("&#39;", "'");
}
