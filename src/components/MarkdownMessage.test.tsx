import { fireEvent, screen } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { MarkdownMessage, memoryDomId } from "./MarkdownMessage";

const SYSTEM_CONTENT =
  "<command-name>/clear</command-name> <command-message>clear</command-message>";

// memory role enum: 1=User, 2=Assistant, 3=System, 4=Tool, 5=Meta,
// 6=Reflection. Bound to a var so biome's a11y rule doesn't read the `role`
// prop as an ARIA role literal.
const ROLE = { user: 1, system: 3, tool: 4, meta: 5, reflection: 6 } as const;

function renderMessage(role: number, content: string, metadata?: string | null) {
  return renderWithProviders(
    <MarkdownMessage role={role} createdAtMs={0} content={content} metadata={metadata} />,
  );
}

describe("MarkdownMessage", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
  });

  it("folds a tag-only message behind a system summary", () => {
    const { container } = renderMessage(ROLE.system, SYSTEM_CONTENT);
    expect(screen.getByText("システムメッセージ")).toBeInTheDocument();
    const pre = container.querySelector("pre.system-fold-body");
    expect(pre).not.toBeNull();
    // Raw tags are shown verbatim inside the <pre>, not parsed as markdown.
    expect(pre?.textContent).toBe(SYSTEM_CONTENT);
  });

  it("expands the fold when the summary is clicked", () => {
    const { container } = renderMessage(ROLE.system, SYSTEM_CONTENT);
    const details = container.querySelector("details.system-fold") as HTMLDetailsElement;
    expect(details.open).toBe(false);
    fireEvent.click(screen.getByText("システムメッセージ"));
    expect(details.open).toBe(true);
  });

  it("renders a normal message without folding", () => {
    const { container } = renderMessage(ROLE.user, "これは普通のテキストです");
    expect(screen.queryByText("システムメッセージ")).toBeNull();
    expect(screen.getByText("これは普通のテキストです")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).toBeNull();
  });

  it("folds Codex-injected AGENTS.md user messages using metadata", () => {
    const metadata = JSON.stringify({
      source: "codex",
      kind: "user",
      payload_type: "message",
      block_type: "input_text",
    });
    const { container } = renderMessage(
      ROLE.user,
      "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\n- x\n</INSTRUCTIONS>",
      metadata,
    );

    expect(screen.getByText("システムメッセージ")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });

  it("folds Codex-injected AGENTS.md user messages without a path suffix", () => {
    const metadata = JSON.stringify({
      source: "codex",
      kind: "user",
      payload_type: "message",
      block_type: "input_text",
    });
    const { container } = renderMessage(
      ROLE.user,
      "# AGENTS.md instructions\n\n<INSTRUCTIONS>\n- x\n</INSTRUCTIONS>",
      metadata,
    );

    expect(screen.getByText("システムメッセージ")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });

  it("folds Codex-injected environment context user messages using metadata", () => {
    const metadata = JSON.stringify({
      source: "codex",
      kind: "user",
      payload_type: "message",
      block_type: "input_text",
    });
    const { container } = renderMessage(
      ROLE.user,
      "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>",
      metadata,
    );

    expect(screen.getByText("システムメッセージ")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });

  it("does not fold an active Codex user event", () => {
    const metadata = JSON.stringify({
      source: "codex",
      kind: "user",
      payload_type: "user_message",
    });
    const { container } = renderMessage(ROLE.user, "AGENTS.md を確認して", metadata);

    expect(screen.queryByText("システムメッセージ")).toBeNull();
    expect(screen.getByText("AGENTS.md を確認して")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).toBeNull();
  });

  it("does not fold a message that has body text outside the tags", () => {
    renderMessage(ROLE.user, "次のコードを実行: <command>ls</command>");
    expect(screen.queryByText("システムメッセージ")).toBeNull();
  });

  it("folds Claude Code external meta prompts using metadata", () => {
    const metadata = JSON.stringify({
      source: "claude_code",
      kind: "user",
      entry_type: "user",
      block_type: "text",
      claude_code: { is_meta: true, user_type: "external" },
    });
    const { container } = renderMessage(ROLE.user, "expanded slash command prompt", metadata);

    expect(screen.getByText("システムメッセージ")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });

  it("does not fold legacy Claude Code prompt-shaped text without claude_code metadata", () => {
    const metadata = JSON.stringify({
      source: "claude_code",
      kind: "user",
      entry_type: "user",
      block_type: "text",
    });
    const content = "<!--\nname: 'Agent Prompt: /review slash command'\n-->\nPrompt body";

    const { container } = renderMessage(ROLE.user, content, metadata);

    expect(screen.queryByText("システムメッセージ")).toBeNull();
    expect(container.querySelector("details.system-fold")).toBeNull();
  });

  it("renders a Claude Code slash command invocation as the command name", () => {
    const metadata = JSON.stringify({
      source: "claude_code",
      kind: "user",
      entry_type: "user",
      block_type: "text",
      claude_code: { user_type: "external" },
    });
    const { container } = renderMessage(
      ROLE.user,
      "<command-message>review</command-message>\n<command-name>/review</command-name>",
      metadata,
    );

    expect(screen.getByText("/review")).toBeInTheDocument();
    expect(screen.queryByText("システムメッセージ")).toBeNull();
    expect(container.querySelector("details.system-fold")).toBeNull();
  });

  it("keeps legacy Claude Code command tags folded without the metadata contract", () => {
    const { container } = renderMessage(
      ROLE.user,
      "<command-message>review</command-message>\n<command-name>/review</command-name>",
      JSON.stringify({ source: "claude_code", kind: "user", entry_type: "user" }),
    );

    expect(screen.getByText("システムメッセージ")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });

  it("folds Claude Code attachment scaffolding using metadata", () => {
    const metadata = JSON.stringify({
      source: "claude_code",
      kind: "attachment",
      entry_type: "attachment",
      subtype: "task_reminder",
      claude_code: { entrypoint: "cli" },
    });
    const { container } = renderMessage(ROLE.user, "task reminder", metadata);

    expect(screen.getByText("システムメッセージ")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });

  it("keeps the existing tool-output fold for tool messages (role 4)", () => {
    const { container } = renderMessage(ROLE.tool, "<tool-result>ok</tool-result>");
    expect(screen.getByText(/Tool 出力/)).toBeInTheDocument();
    expect(screen.queryByText("システムメッセージ")).toBeNull();
    expect(container.querySelector("details.tool-details")).not.toBeNull();
  });

  it("treats an empty string as a normal (non-folded) message", () => {
    const { container } = renderMessage(ROLE.system, "");
    expect(screen.queryByText("システムメッセージ")).toBeNull();
    expect(container.querySelector("details.system-fold")).toBeNull();
  });

  it("folds a Meta-role message even when its content is plain text", () => {
    // role=5 (Meta) is codex 'developer' / claude-code 'system' / token_count
    // etc. — always system scaffolding, so fold regardless of content shape.
    const { container } = renderMessage(ROLE.meta, "# Basic Principles\n- Respond in Japanese");
    expect(screen.getByText("システムメッセージ")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });

  it("folds a Meta-role message whose content is a JSON meta-event", () => {
    const { container } = renderMessage(ROLE.meta, '{"info":{"last_token_usage":{}}}');
    expect(screen.getByText("システムメッセージ")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });

  it("folds a Reflection-role message (role 6)", () => {
    const { container } = renderMessage(ROLE.reflection, "self-reflection summary");
    expect(screen.getByText("システムメッセージ")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });

  it("labels the Meta role instead of '?'", () => {
    renderMessage(ROLE.meta, "x");
    expect(screen.getByText("Meta")).toBeInTheDocument();
    expect(screen.queryByText("?")).toBeNull();
  });

  it("labels the Reflection role instead of '?'", () => {
    renderMessage(ROLE.reflection, "x");
    expect(screen.getByText("Reflection")).toBeInTheDocument();
    expect(screen.queryByText("?")).toBeNull();
  });

  it("sets the scroll-anchor DOM id from memoryId", () => {
    const { container } = renderWithProviders(
      <MarkdownMessage role={ROLE.user} createdAtMs={0} content="hi" memoryId="42" />,
    );
    expect(container.querySelector(`#${CSS.escape(memoryDomId("42"))}`)).not.toBeNull();
  });

  it("flashes the hit row only when highlight is set", () => {
    const { container, rerender } = renderWithProviders(
      <MarkdownMessage role={ROLE.user} createdAtMs={0} content="hi" memoryId="42" />,
    );
    expect(container.querySelector(".message-hit")).toBeNull();
    // The provider tree must be re-applied on rerender — `renderWithProviders`
    // wraps via inline JSX (not the `wrapper` option), so a bare element would
    // drop the i18n context and crash `useTranslation`.
    rerender(
      <I18nextProvider i18n={i18n}>
        <MarkdownMessage role={ROLE.user} createdAtMs={0} content="hi" memoryId="42" highlight />
      </I18nextProvider>,
    );
    expect(container.querySelector(".message-hit")).not.toBeNull();
  });

  it("anchors and highlights folded (system) rows too", () => {
    const { container } = renderWithProviders(
      <MarkdownMessage role={ROLE.meta} createdAtMs={0} content="{}" memoryId="7" highlight />,
    );
    const row = container.querySelector(`#${CSS.escape(memoryDomId("7"))}`);
    expect(row).not.toBeNull();
    expect(row?.classList.contains("message-hit")).toBe(true);
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });
});
