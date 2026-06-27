import { describe, expect, it } from "vitest";
import type { MemoryRow } from "@/types/api";
import { isCodexInjectedUserMessage, visibleConversationMemories } from "./codexMemoryVisibility";

function memory(
  id: string,
  role: number,
  content: string,
  metadata?: Record<string, unknown>,
): MemoryRow {
  return {
    id,
    role,
    content_type: 0,
    content,
    created_at_ms: 0,
    metadata: metadata ? JSON.stringify(metadata) : null,
    external_id: null,
  };
}

const assistantMessage = {
  source: "codex",
  kind: "assistant",
  payload_type: "message",
  block_type: "output_text",
};

const assistantEvent = {
  source: "codex",
  kind: "assistant",
  payload_type: "agent_message",
};

const userMessage = {
  source: "codex",
  kind: "user",
  payload_type: "message",
  block_type: "input_text",
};

const userEvent = {
  source: "codex",
  kind: "user",
  payload_type: "user_message",
};

describe("visibleConversationMemories", () => {
  it("prefers the Codex user event over a duplicate response_item user message", () => {
    const out = visibleConversationMemories([
      memory("1", 1, "typed by user", userMessage),
      memory("2", 1, "typed by user", userEvent),
    ]);

    expect(out.map((m) => m.id)).toEqual(["2"]);
  });

  it("keeps Codex injected response_item user messages when no user event exists", () => {
    const out = visibleConversationMemories([
      memory("1", 1, "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>...", userMessage),
    ]);

    expect(out.map((m) => m.id)).toEqual(["1"]);
  });

  it("hides a neighboring Codex assistant event when a canonical message exists", () => {
    const out = visibleConversationMemories([
      memory("1", 2, "same", assistantEvent),
      memory("2", 2, "same", assistantMessage),
    ]);

    expect(out.map((m) => m.id)).toEqual(["2"]);
  });

  it("keeps a Codex user event as fallback when no canonical message exists", () => {
    const out = visibleConversationMemories([memory("1", 1, "fallback", userEvent)]);

    expect(out.map((m) => m.id)).toEqual(["1"]);
  });

  it("does not hide distant duplicate Codex user response_items", () => {
    const out = visibleConversationMemories([
      memory("1", 1, "same", userMessage),
      memory("2", 1, "noise"),
      memory("3", 1, "noise"),
      memory("4", 1, "noise"),
      memory("5", 1, "noise"),
      memory("6", 1, "noise"),
      memory("7", 1, "same", userEvent),
    ]);

    expect(out.map((m) => m.id)).toEqual(["1", "2", "3", "4", "5", "6", "7"]);
  });

  it("keeps a Codex display event as fallback when no canonical message exists nearby", () => {
    const out = visibleConversationMemories([memory("1", 2, "fallback", assistantEvent)]);

    expect(out.map((m) => m.id)).toEqual(["1"]);
  });

  it("does not dedup repeated content across the whole thread", () => {
    const out = visibleConversationMemories([
      memory("1", 2, "OK", assistantMessage),
      memory("2", 2, "noise"),
      memory("3", 2, "noise"),
      memory("4", 2, "noise"),
      memory("5", 2, "noise"),
      memory("6", 2, "noise"),
      memory("7", 2, "OK", assistantEvent),
    ]);

    expect(out.map((m) => m.id)).toEqual(["1", "2", "3", "4", "5", "6", "7"]);
  });

  it("leaves non-Codex and metadata-less memories visible", () => {
    const out = visibleConversationMemories([
      memory("1", 2, "same", {
        source: "claude-code",
        kind: "assistant",
        payload_type: "agent_message",
      }),
      memory("2", 2, "same"),
    ]);

    expect(out.map((m) => m.id)).toEqual(["1", "2"]);
  });
});

describe("isCodexInjectedUserMessage", () => {
  const userMeta = JSON.stringify(userMessage);

  it("folds the AGENTS.md heading + <INSTRUCTIONS> block injection", () => {
    expect(
      isCodexInjectedUserMessage(
        "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\n- x\n</INSTRUCTIONS>",
        userMeta,
      ),
    ).toBe(true);
  });

  it("folds the CLAUDE.md heading + <INSTRUCTIONS> block injection", () => {
    expect(
      isCodexInjectedUserMessage(
        "# CLAUDE.md instructions\n\n<INSTRUCTIONS>\n- x\n</INSTRUCTIONS>",
        userMeta,
      ),
    ).toBe(true);
  });

  it("does NOT fold a user question that opens with an AGENTS.md-shaped heading", () => {
    // Regression: previously the bare `startsWith("# AGENTS.md instructions")`
    // matched user-typed questions whose first markdown heading mentioned the
    // file, hiding real conversation behind the system-fold.
    expect(
      isCodexInjectedUserMessage(
        "# AGENTS.md instructions are documented at docs/agents.md — どこで読める?",
        userMeta,
      ),
    ).toBe(false);
  });

  it("does NOT fold a user question that mentions CLAUDE.md without the injection block", () => {
    expect(
      isCodexInjectedUserMessage("# CLAUDE.md instructions\n\nこのファイルの所有者は誰?", userMeta),
    ).toBe(false);
  });

  it("folds <environment_context>, <permissions instructions>, <turn_aborted> without an INSTRUCTIONS block", () => {
    for (const head of [
      "<environment_context>\n<cwd>/repo</cwd>\n</environment_context>",
      "<permissions instructions>\nrw\n</permissions instructions>",
      "<turn_aborted>reason</turn_aborted>",
    ]) {
      expect(isCodexInjectedUserMessage(head, userMeta)).toBe(true);
    }
  });

  it("ignores messages without the Codex response_item metadata", () => {
    expect(
      isCodexInjectedUserMessage(
        "# AGENTS.md instructions\n\n<INSTRUCTIONS>x</INSTRUCTIONS>",
        null,
      ),
    ).toBe(false);
    expect(
      isCodexInjectedUserMessage(
        "# AGENTS.md instructions\n\n<INSTRUCTIONS>x</INSTRUCTIONS>",
        JSON.stringify(userEvent),
      ),
    ).toBe(false);
  });
});
