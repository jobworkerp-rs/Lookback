import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { MemoryRow, ThreadSummary } from "@/types/api";
import { memoryDomId } from "./MarkdownMessage";
import { ThreadDetail } from "./ThreadDetail";

const findMemoriesByThreadId = vi.fn();
const findThread = vi.fn();
vi.mock("@/api", () => ({
  findMemoriesByThreadId: (req: unknown) => findMemoriesByThreadId(req),
  findThread: (id: string) => findThread(id),
}));

// jsdom lacks both APIs the component relies on.
beforeAll(() => {
  globalThis.IntersectionObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
    takeRecords() {
      return [];
    }
    root = null;
    rootMargin = "";
    thresholds = [];
  } as unknown as typeof IntersectionObserver;
  Element.prototype.scrollIntoView = vi.fn();
});

function row(id: string, content: string, role = 2, metadata?: Record<string, unknown>): MemoryRow {
  return {
    id,
    role,
    content_type: 0,
    content,
    created_at_ms: 0,
    metadata: metadata ? JSON.stringify(metadata) : null,
  };
}

const thread: ThreadSummary = {
  id: "10",
  user_id: "1",
  description: "Thread T",
  channel: null,
  labels: [],
  created_at_ms: 0,
  updated_at_ms: 0,
};

describe("ThreadDetail", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    findThread.mockReset();
    // Default: the synthesized-summary hydration finds nothing, so the header
    // falls back to the prop (matches the pre-hydration behaviour these tests
    // were written against).
    findThread.mockResolvedValue(null);
  });

  it("renders the first page of memories", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hello"), row("2", "world")]);
    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);
    expect(await screen.findByText("hello")).toBeInTheDocument();
    expect(screen.getByText("world")).toBeInTheDocument();
  });

  it("requests the centered offset for a search hit", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("7", "hit row")]);
    renderWithProviders(
      <ThreadDetail
        thread={thread}
        highlight={{ memoryId: "7", position: 350 }}
        onClose={() => {}}
      />,
    );
    await screen.findByText("hit row");
    // initialOffset(350) with PAGE_SIZE 100 => 300.
    expect(findMemoriesByThreadId).toHaveBeenCalledWith({
      thread_id: "10",
      limit: 100,
      offset: 300,
    });
  });

  it("marks the hit row with the highlight anchor id", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("7", "hit row")]);
    const { container } = renderWithProviders(
      <ThreadDetail
        thread={thread}
        highlight={{ memoryId: "7", position: 0 }}
        onClose={() => {}}
      />,
    );
    await screen.findByText("hit row");
    const el = container.querySelector(`#${CSS.escape(memoryDomId("7"))}`);
    expect(el).not.toBeNull();
    expect(el?.classList.contains("message-hit")).toBe(true);
  });

  it("shows the empty state when the thread has no memories", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([]);
    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);
    await waitFor(() => expect(screen.getByText("メッセージが見つかりません")).toBeInTheDocument());
  });

  it("shows only User and Assistant roles by default", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([
      row("1", "user message", 1),
      row("2", "assistant message", 2),
      row("3", "system message", 3),
      row("4", "tool message", 4),
      row("5", "meta message", 5),
      row("6", "reflection message", 6),
      row("7", "unspecified message", 0),
    ]);

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    expect(await screen.findByText("user message")).toBeInTheDocument();
    expect(screen.getByText("assistant message")).toBeInTheDocument();
    expect(screen.queryByText("system message")).toBeNull();
    expect(screen.queryByText("tool message")).toBeNull();
    expect(screen.queryByText("meta message")).toBeNull();
    expect(screen.queryByText("reflection message")).toBeNull();
    expect(screen.queryByText("unspecified message")).toBeNull();
    expect(screen.getByRole("button", { name: "User" })).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByRole("button", { name: "Assistant" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    expect(screen.getByRole("button", { name: "Tool" })).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByRole("button", { name: "Meta" })).toHaveAttribute("aria-pressed", "false");
  });

  it("toggles role visibility from the header controls", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([
      row("1", "user message", 1),
      row("2", "tool message", 4),
    ]);

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    expect(await screen.findByText("user message")).toBeInTheDocument();
    expect(screen.queryByText("tool message")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Tool" }));
    expect(screen.getByText("tool message")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Tool" })).toHaveAttribute("aria-pressed", "true");

    fireEvent.click(screen.getByRole("button", { name: "Tool" }));
    expect(screen.queryByText("tool message")).toBeNull();
    expect(screen.getByRole("button", { name: "Tool" })).toHaveAttribute("aria-pressed", "false");

    fireEvent.click(screen.getByRole("button", { name: "User" }));
    expect(screen.queryByText("user message")).toBeNull();
    expect(screen.getByRole("button", { name: "User" })).toHaveAttribute("aria-pressed", "false");
  });

  it("keeps a highlighted memory visible even when its role is hidden by default", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("7", "tool hit", 4)]);
    const { container } = renderWithProviders(
      <ThreadDetail
        thread={thread}
        highlight={{ memoryId: "7", position: 0 }}
        onClose={() => {}}
      />,
    );

    await screen.findByText(/Tool 出力/);
    const el = container.querySelector(`#${CSS.escape(memoryDomId("7"))}`);
    expect(el).not.toBeNull();
    expect(el?.classList.contains("message-hit")).toBe(true);
    expect(screen.getByRole("button", { name: "Tool" })).toHaveAttribute("aria-pressed", "false");
  });

  it("distinguishes empty filtered results from an empty thread", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "tool only", 4)]);

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    await waitFor(() =>
      expect(
        screen.getByText("表示中の Role に一致するメッセージがありません"),
      ).toBeInTheDocument(),
    );
    expect(screen.queryByText("メッセージが見つかりません")).toBeNull();
  });

  it("folds Codex injected user memories in the detail list", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([
      row("1", "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>x</INSTRUCTIONS>", 1, {
        source: "codex",
        kind: "user",
        payload_type: "message",
        block_type: "input_text",
      }),
    ]);

    const { container } = renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    expect(await screen.findByText("システムメッセージ")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });

  it("folds Codex injected AGENTS.md memories without a path suffix in the detail list", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([
      row("1", "# AGENTS.md instructions\n\n<INSTRUCTIONS>x</INSTRUCTIONS>", 1, {
        source: "codex",
        kind: "user",
        payload_type: "message",
        block_type: "input_text",
      }),
    ]);

    const { container } = renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    expect(await screen.findByText("システムメッセージ")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });

  it("hydrates channel/labels from find_thread when opened with a synthesized summary", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hi")]);
    // Cross-tab entry: prop has empty channel/labels; the fetched row fills them.
    findThread.mockResolvedValueOnce({
      ...thread,
      channel: "codex",
      // Deliberately out of priority order to prove the header sorts them.
      labels: ["dir:/proj", "agent:codex"],
    });

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    await screen.findByText("hi");
    expect(findThread).toHaveBeenCalledWith("10");
    // sortLabelsByPrefixPriority puts agent before dir.
    expect(
      await screen.findByText((_, el) => el?.textContent === "codex · agent:codex, dir:/proj"),
    ).toBeInTheDocument();
  });

  it("does not fetch the thread row when the prop already carries channel/labels", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hi")]);
    const full: ThreadSummary = { ...thread, channel: "codex", labels: ["agent:codex"] };

    renderWithProviders(<ThreadDetail thread={full} onClose={() => {}} />);

    await screen.findByText("hi");
    expect(findThread).not.toHaveBeenCalled();
    expect(
      screen.getByText((_, el) => el?.textContent === "codex · agent:codex"),
    ).toBeInTheDocument();
  });
});
