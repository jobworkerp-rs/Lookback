import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { ChatTurn, UseRagChat } from "@/hooks/useRagChat";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { ChatSource } from "@/types/api";
import { Chat } from "./Chat";

// `findMemoryPosition` is overridden per-test to flip between the
// success and fallback branches of `resolveThreadHighlight`.
// `findMemoriesByThreadId` is what ThreadDetail calls once mounted;
// keep it minimal so the modal renders without errors.
vi.mock("@/api", () => ({
  findMemoryPosition: vi.fn(),
  findMemoriesByThreadId: vi.fn().mockResolvedValue([]),
}));

import { findMemoryPosition } from "@/api";

const mockFindMemoryPosition = vi.mocked(findMemoryPosition);

beforeAll(() => {
  // ThreadDetail relies on these jsdom-absent APIs.
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

function turn(over: Partial<ChatTurn> & { sources: ChatSource[] }): ChatTurn {
  return {
    jobId: "job-1",
    question: "test question",
    answer: "test answer",
    phase: "done",
    message: null,
    error: null,
    ...over,
  };
}

function makeRag(turns: ChatTurn[], overrides: Partial<UseRagChat> = {}): UseRagChat {
  return {
    turns,
    ask: vi.fn().mockResolvedValue(undefined),
    reset: vi.fn(),
    cancel: vi.fn().mockResolvedValue(undefined),
    busy: false,
    ...overrides,
  };
}

interface RenderOpts {
  onNavigate?: ReturnType<typeof vi.fn>;
  onNavigateSummariesFocus?: ReturnType<typeof vi.fn>;
}

function renderChat(sources: ChatSource[], opts: RenderOpts = {}) {
  const onNavigate = opts.onNavigate ?? vi.fn();
  const onNavigateSummariesFocus = opts.onNavigateSummariesFocus ?? vi.fn();
  renderWithProviders(
    <Chat
      onNavigate={onNavigate}
      onNavigateSummariesFocus={onNavigateSummariesFocus}
      rag={makeRag([turn({ sources })])}
    />,
  );
  return { onNavigate, onNavigateSummariesFocus };
}

beforeEach(() => {
  i18n.changeLanguage("ja");
  mockFindMemoryPosition.mockReset();
  // Default to the fallback branch (memoryId-only highlight); tests
  // that exercise the success path override this.
  mockFindMemoryPosition.mockResolvedValue(null);
});

describe("Chat source click", () => {
  it("opens ThreadDetail in-place for raw_memory sources", async () => {
    const rawSource: ChatSource = {
      source_kind: "raw_memory",
      memory_id: "100",
      source_thread_id: "200",
      snippet: "snippet",
      score: 0.9,
    };
    const { onNavigate, onNavigateSummariesFocus } = renderChat([rawSource]);
    fireEvent.click(screen.getByTitle(/^原文:/));
    await waitFor(() => {
      expect(screen.getByRole("dialog")).toBeTruthy();
    });
    expect(onNavigate).not.toHaveBeenCalled();
    expect(onNavigateSummariesFocus).not.toHaveBeenCalled();
  });

  it("opens ThreadDetail in-place for thread_summary sources", async () => {
    const summarySource: ChatSource = {
      source_kind: "thread_summary",
      memory_id: "101",
      source_thread_id: "201",
      snippet: "summary snippet",
      score: 0.8,
    };
    const { onNavigate, onNavigateSummariesFocus } = renderChat([summarySource]);
    fireEvent.click(screen.getByTitle(/^スレッド要約:/));
    await waitFor(() => {
      expect(screen.getByRole("dialog")).toBeTruthy();
    });
    expect(onNavigate).not.toHaveBeenCalled();
    expect(onNavigateSummariesFocus).not.toHaveBeenCalled();
  });

  it("passes the resolved position into ThreadDetail when findMemoryPosition succeeds", async () => {
    mockFindMemoryPosition.mockResolvedValue({ position: 5, thread_total: 20 });
    const rawSource: ChatSource = {
      source_kind: "raw_memory",
      memory_id: "300",
      source_thread_id: "400",
      snippet: "snippet",
      score: 0.9,
    };
    renderChat([rawSource]);
    fireEvent.click(screen.getByTitle(/^原文:/));
    await waitFor(() => {
      expect(screen.getByRole("dialog")).toBeTruthy();
    });
    expect(mockFindMemoryPosition).toHaveBeenCalledWith({
      thread_id: "400",
      memory_id: "300",
    });
  });

  it("deep-links monthly period_summary sources into the Summaries calendar", () => {
    const periodSource: ChatSource = {
      source_kind: "period_summary",
      memory_id: "102",
      period_key: "2026-05",
      scope_key: "_all",
      snippet: "period snippet",
      score: 0.7,
    };
    const { onNavigate, onNavigateSummariesFocus } = renderChat([periodSource]);
    fireEvent.click(screen.getByTitle(/^期間要約:/));
    expect(onNavigateSummariesFocus).toHaveBeenCalledWith({
      kind: "monthly",
      month: "2026-05",
      periodKey: "2026-05",
    });
    expect(onNavigate).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("deep-links daily period_summary sources into the matching month", () => {
    const periodSource: ChatSource = {
      source_kind: "period_summary",
      memory_id: "103",
      period_key: "2026-05-28",
      scope_key: "project-x",
      snippet: "period snippet",
      score: 0.7,
    };
    const { onNavigateSummariesFocus } = renderChat([periodSource]);
    fireEvent.click(screen.getByTitle(/^期間要約:/));
    expect(onNavigateSummariesFocus).toHaveBeenCalledWith({
      kind: "daily",
      month: "2026-05",
      periodKey: "2026-05-28",
    });
  });

  it("falls back to a plain Summaries tab switch when period_key is malformed", () => {
    const periodSource: ChatSource = {
      source_kind: "period_summary",
      memory_id: "104",
      period_key: "garbage",
      scope_key: "_all",
      snippet: "period snippet",
      score: 0.7,
    };
    const { onNavigate, onNavigateSummariesFocus } = renderChat([periodSource]);
    fireEvent.click(screen.getByTitle(/^期間要約:/));
    expect(onNavigate).toHaveBeenCalledWith("summaries");
    expect(onNavigateSummariesFocus).not.toHaveBeenCalled();
  });
});

describe("Chat composer stop button — OPEN-CHAT-2", () => {
  function renderComposer(rag: UseRagChat) {
    renderWithProviders(<Chat onNavigate={vi.fn()} onNavigateSummariesFocus={vi.fn()} rag={rag} />);
  }

  it("shows the stop button instead of submit while busy", () => {
    const inFlight: ChatTurn = {
      jobId: "job-busy",
      question: "thinking out loud",
      answer: "thi",
      sources: [],
      phase: "token",
      message: null,
      error: null,
    };
    renderComposer(makeRag([inFlight], { busy: true }));
    expect(screen.queryByRole("button", { name: "送信" })).toBeNull();
    expect(screen.getByRole("button", { name: "停止" })).toBeTruthy();
  });

  it("invokes rag.cancel when the user clicks stop", () => {
    const inFlight: ChatTurn = {
      jobId: "job-busy",
      question: "thinking out loud",
      answer: "thi",
      sources: [],
      phase: "token",
      message: null,
      error: null,
    };
    const cancel = vi.fn().mockResolvedValue(undefined);
    renderComposer(makeRag([inFlight], { busy: true, cancel }));
    fireEvent.click(screen.getByRole("button", { name: "停止" }));
    expect(cancel).toHaveBeenCalledTimes(1);
  });

  it("restores the submit button once the turn settles", () => {
    const settled: ChatTurn = {
      jobId: "job-done",
      question: "ok",
      answer: "answer",
      sources: [],
      phase: "done",
      message: null,
      error: null,
    };
    renderComposer(makeRag([settled], { busy: false }));
    expect(screen.queryByRole("button", { name: "停止" })).toBeNull();
    expect(screen.getByRole("button", { name: "送信" })).toBeTruthy();
  });
});

describe("Chat source list collapse", () => {
  // Prefix the memory_id with the kind so concatenating breakdowns
  // doesn't accidentally produce duplicate React keys. ChatSource is a
  // discriminated union, so each branch returns the shape that matches
  // its kind.
  function makeSources(
    count: number,
    kind: ChatSource["source_kind"] = "raw_memory",
  ): ChatSource[] {
    return Array.from({ length: count }, (_, i): ChatSource => {
      const memory_id = `${kind}-${i}`;
      const snippet = `snippet ${i}`;
      const score = 0.5;
      if (kind === "period_summary") {
        return {
          source_kind: "period_summary",
          memory_id,
          period_key: `2026-05-${String(i % 28).padStart(2, "0")}`,
          scope_key: "_all",
          snippet,
          score,
        };
      }
      return {
        source_kind: kind,
        memory_id,
        source_thread_id: `t-${kind}-${i}`,
        snippet,
        score,
      };
    });
  }

  it("renders the source list collapsed by default so 10 entries don't dominate the viewport", () => {
    renderChat(makeSources(10));
    // <details> defaults to closed in jsdom too; verify via the
    // attribute the browser exposes.
    const details = screen.getByText(/^出典 \(10\)$/).closest("details");
    expect(details).toBeTruthy();
    expect(details?.hasAttribute("open")).toBe(false);
  });

  it("breaks down the count by kind when the list mixes sources", () => {
    renderChat([
      ...makeSources(3, "raw_memory"),
      ...makeSources(2, "thread_summary"),
      ...makeSources(1, "period_summary"),
    ]);
    // The single-line summary must let the user know what's inside
    // before they commit to expanding the list.
    expect(screen.getByText("原文 3 / スレッド要約 2 / 期間要約 1")).toBeTruthy();
  });

  it("omits the breakdown when every source is the same kind", () => {
    renderChat(makeSources(5, "raw_memory"));
    // "原文 5" would just repeat the "出典 (5)" count — drop it as noise.
    expect(screen.queryByText(/^原文 \d+/)).toBeNull();
    expect(screen.getByText("出典 (5)")).toBeTruthy();
  });
});
