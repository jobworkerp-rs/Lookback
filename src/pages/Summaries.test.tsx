import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { SidecarStatus } from "@/hooks/useSidecarStatus";
import type { StepStreamProgressHandle } from "@/hooks/useStepStreamProgress";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { ResolvedSummaryMemoryRef, SummaryEntry } from "@/types/api";
import { periodKeyPrefixesForMonth, Summaries } from "./Summaries";

// Mock the API surface that Summaries (and its descendants via
// `resolveSummaryRefNavigation` / `ThreadDetail`) hits during render.
vi.mock("@/api", () => ({
  listSummaries: vi.fn(),
  listSummaryPeriodKeys: vi.fn().mockResolvedValue([]),
  findSummaryDistinctLabels: vi.fn().mockResolvedValue([]),
  findSummaryCoOccurringLabels: vi.fn().mockResolvedValue([]),
  deleteSummary: vi.fn(),
  findMemoryPosition: vi.fn().mockResolvedValue(null),
  findMemoriesByThreadId: vi.fn().mockResolvedValue([]),
  resolveSummaryMemoryRef: vi.fn(),
  // Search-mode registry pulls these in eagerly.
  searchMemoriesKeyword: vi.fn().mockResolvedValue([]),
  searchMemoriesSemantic: vi.fn().mockResolvedValue([]),
  searchMemoriesHybrid: vi.fn().mockResolvedValue([]),
  parseSummaryContent: ({ content_json }: { content_json: string }) => {
    try {
      const parsed = JSON.parse(content_json);
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        return { parsed, raw: content_json };
      }
    } catch {
      // legacy plain-text body
    }
    return { parsed: null, raw: content_json };
  },
}));

import {
  findSummaryCoOccurringLabels,
  findSummaryDistinctLabels,
  listSummaries,
  listSummaryPeriodKeys,
  resolveSummaryMemoryRef,
  searchMemoriesKeyword,
} from "@/api";

const mockListSummaries = vi.mocked(listSummaries);
const mockListSummaryPeriodKeys = vi.mocked(listSummaryPeriodKeys);
const mockResolveSummaryMemoryRef = vi.mocked(resolveSummaryMemoryRef);
const mockFindSummaryDistinctLabels = vi.mocked(findSummaryDistinctLabels);
const mockFindSummaryCoOccurringLabels = vi.mocked(findSummaryCoOccurringLabels);
const mockSearchMemoriesKeyword = vi.mocked(searchMemoriesKeyword);

beforeAll(() => {
  // ThreadDetail uses IntersectionObserver / scrollIntoView, which jsdom lacks.
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

beforeEach(() => {
  i18n.changeLanguage("ja");
  mockListSummaries.mockReset();
  mockListSummaryPeriodKeys.mockReset();
  mockResolveSummaryMemoryRef.mockReset();
  mockFindSummaryDistinctLabels.mockReset();
  mockFindSummaryCoOccurringLabels.mockReset();
  mockSearchMemoriesKeyword.mockReset();
  mockFindSummaryDistinctLabels.mockResolvedValue([]);
  mockFindSummaryCoOccurringLabels.mockResolvedValue([]);
  mockListSummaryPeriodKeys.mockResolvedValue([]);
  mockSearchMemoriesKeyword.mockResolvedValue([]);
});

function noopProgress(): StepStreamProgressHandle {
  return {
    progress: null,
    busy: false,
    start: vi.fn(),
    clear: vi.fn(),
    cancel: vi.fn().mockResolvedValue(undefined),
  };
}

function busyProgress(overrides: Partial<StepStreamProgressHandle> = {}): StepStreamProgressHandle {
  return {
    progress: { job_id: "summary-1", status: "active", message: "running" },
    busy: true,
    start: vi.fn(),
    clear: vi.fn(),
    cancel: vi.fn().mockResolvedValue(undefined),
    ...overrides,
  };
}

const readySidecar: SidecarStatus = {
  phase: "ready",
  warnings: [],
  endpoints: {
    jobworkerp_port: 9000,
    memories_port: 9010,
    conductor_port: 9020,
    mcp_server_port: null,
  },
};

function perThreadEntry(over: Partial<SummaryEntry> = {}): SummaryEntry {
  return {
    memory_id: "999",
    thread_id: "42",
    external_id: "summary:42",
    kind: "per-thread",
    period_key: null,
    scope_key: null,
    content_json: JSON.stringify({
      title: "Coding session",
      summary: "Did the thing.",
      source_memory_ids: ["111", "222"],
    }),
    updated_at_ms: Date.now(),
    ...over,
  };
}

function renderSummaries() {
  return renderWithProviders(<Summaries summaryProgress={noopProgress()} sidecar={readySidecar} />);
}

function renderCalendar(kind: "daily" | "weekly" | "monthly", month = "2026-07") {
  return renderWithProviders(
    <Summaries
      summaryProgress={noopProgress()}
      sidecar={readySidecar}
      focus={{ kind, month, periodKey: "" }}
    />,
  );
}

const summaryLabel = (label: string, count: number) =>
  screen.getByRole("button", { name: `${label} (${count})` });

async function openSummaryLabelBar() {
  const summary = await screen.findByText(/^ラベルで絞り込む/);
  fireEvent.click(summary);
}

describe("Summaries calendar", () => {
  it("uses period-key prefixes for daily, weekly, and monthly calendar windows", () => {
    expect(periodKeyPrefixesForMonth("daily", "2026-07")).toEqual(["2026-07-"]);
    expect(periodKeyPrefixesForMonth("monthly", "2026-07")).toEqual(["2026-07"]);
    expect(periodKeyPrefixesForMonth("weekly", "2026-07")).toEqual([
      "2026-W27",
      "2026-W28",
      "2026-W29",
      "2026-W30",
      "2026-W31",
    ]);
  });

  it("discovers daily keys by period-key prefix instead of updated_at", async () => {
    renderCalendar("daily");

    await waitFor(() => {
      expect(mockListSummaryPeriodKeys).toHaveBeenCalledWith({
        kind: "daily",
        period_key_prefixes: ["2026-07-"],
      });
    });
  });
});

describe("Summaries per-thread label filter", () => {
  it("passes summary plus selected labels with ALL matching to the list", async () => {
    mockListSummaries.mockResolvedValue([perThreadEntry()]);
    mockFindSummaryDistinctLabels.mockResolvedValue([{ label: "agent:codex", thread_count: 3 }]);
    renderSummaries();

    await openSummaryLabelBar();
    fireEvent.click(summaryLabel("agent:codex", 3));

    await waitFor(() => {
      expect(mockListSummaries).toHaveBeenLastCalledWith(
        expect.objectContaining({ labels_any: ["summary", "agent:codex"] }),
      );
    });
  });

  it("uses the same fixed ALL filter for keyword search", async () => {
    mockListSummaries.mockResolvedValue([perThreadEntry()]);
    mockFindSummaryDistinctLabels.mockResolvedValue([{ label: "agent:codex", thread_count: 3 }]);
    renderSummaries();

    await openSummaryLabelBar();
    fireEvent.click(summaryLabel("agent:codex", 3));
    fireEvent.click(screen.getByRole("button", { name: "検索" }));
    fireEvent.click(screen.getByRole("button", { name: "Keyword" }));
    fireEvent.change(screen.getByPlaceholderText("検索クエリ"), { target: { value: "review" } });

    await waitFor(() => {
      expect(mockSearchMemoriesKeyword).toHaveBeenLastCalledWith(
        expect.objectContaining({
          user_id: 1,
          memory_kinds: [2],
          labels_any: ["summary", "agent:codex"],
          label_match: "all",
        }),
      );
    });
  });

  it.each([
    ["日次", 3],
    ["週次", 4],
    ["月次", 5],
  ])("searches %s summaries by their memory kind", async (kindLabel, memoryKind) => {
    mockListSummaries.mockResolvedValue([perThreadEntry()]);
    renderSummaries();

    fireEvent.click(screen.getByRole("button", { name: kindLabel }));
    fireEvent.click(screen.getByRole("button", { name: "検索" }));
    fireEvent.click(screen.getByRole("button", { name: "Keyword" }));
    fireEvent.change(screen.getByPlaceholderText("検索クエリ"), { target: { value: "review" } });

    await waitFor(() => {
      expect(mockSearchMemoriesKeyword).toHaveBeenLastCalledWith(
        expect.objectContaining({ user_id: 1, memory_kinds: [memoryKind] }),
      );
    });
  });

  it("hides the filter and clears its selection for period summaries", async () => {
    mockListSummaries.mockResolvedValue([perThreadEntry()]);
    mockFindSummaryDistinctLabels.mockResolvedValue([{ label: "agent:codex", thread_count: 3 }]);
    renderSummaries();

    await openSummaryLabelBar();
    fireEvent.click(summaryLabel("agent:codex", 3));
    fireEvent.click(screen.getByRole("button", { name: "日次" }));

    expect(screen.queryByText(/^ラベルで絞り込む/)).toBeNull();
    await waitFor(() => {
      const request = mockListSummaries.mock.calls.at(-1)?.[0];
      expect(request).toMatchObject({ kind: "daily" });
      expect(request).not.toHaveProperty("labels_any");
      expect(request).not.toHaveProperty("label_match");
    });
  });
});

describe("Summaries per-thread Thread link", () => {
  it("renders Thread #{id} as a button and opens ThreadDetail on click", async () => {
    mockListSummaries.mockResolvedValue([perThreadEntry()]);
    renderSummaries();
    const link = await screen.findByRole("button", { name: /^スレッド #42$/ });
    fireEvent.click(link);
    await waitFor(() => {
      expect(screen.getByRole("dialog")).toBeTruthy();
    });
  });
});

describe("Summaries source_memory_ids chip", () => {
  it("opens ThreadDetail when the resolved memory is a per-thread summary", async () => {
    mockListSummaries.mockResolvedValue([perThreadEntry()]);
    const resolved: ResolvedSummaryMemoryRef = {
      memory_id: "111",
      thread_id: "555",
      external_id: "summary:555",
      kind: "per-thread",
      period_key: null,
      scope_key: null,
    };
    mockResolveSummaryMemoryRef.mockResolvedValue(resolved);
    renderSummaries();
    // Two source_memory_ids in the content → two chips (numbered 1 / 2).
    const chips = await screen.findAllByTitle(/^memory /);
    expect(chips).toHaveLength(2);
    fireEvent.click(chips[0] as HTMLElement);
    await waitFor(() => {
      expect(mockResolveSummaryMemoryRef).toHaveBeenCalledWith("111");
    });
    await waitFor(() => {
      expect(screen.getByRole("dialog")).toBeTruthy();
    });
  });

  it("leaves the modal closed when the cited memory cannot be resolved", async () => {
    mockListSummaries.mockResolvedValue([perThreadEntry()]);
    mockResolveSummaryMemoryRef.mockResolvedValue(null);
    renderSummaries();
    const chips = await screen.findAllByTitle(/^memory /);
    fireEvent.click(chips[0] as HTMLElement);
    await waitFor(() => {
      expect(mockResolveSummaryMemoryRef).toHaveBeenCalled();
    });
    // No modal should have been opened.
    expect(screen.queryByRole("dialog")).toBeNull();
  });
});

describe("Summaries generate-button cancel UX", () => {
  // Same pattern as Chat composer (`src/pages/Chat.test.tsx`): while busy
  // the primary slot is replaced by a single 停止 button; clicking it
  // calls the progress handle's cancel; once settled the 生成 button
  // returns. The exact-text matchers below also serve as a drift guard
  // against accidental label tweaks.
  it("shows 停止 instead of 生成 while a generation is in flight", async () => {
    mockListSummaries.mockResolvedValue([]);
    const handle = busyProgress();
    renderWithProviders(<Summaries summaryProgress={handle} sidecar={readySidecar} />);
    expect(screen.queryByRole("button", { name: "生成" })).toBeNull();
    expect(screen.getByRole("button", { name: "停止" })).toBeTruthy();
  });

  it("invokes summaryProgress.cancel when the user clicks 停止", async () => {
    mockListSummaries.mockResolvedValue([]);
    const cancel = vi.fn().mockResolvedValue(undefined);
    const handle = busyProgress({ cancel });
    renderWithProviders(<Summaries summaryProgress={handle} sidecar={readySidecar} />);
    fireEvent.click(screen.getByRole("button", { name: "停止" }));
    await waitFor(() => expect(cancel).toHaveBeenCalledTimes(1));
  });

  it("restores 生成 once the dispatch settles", async () => {
    mockListSummaries.mockResolvedValue([]);
    renderWithProviders(<Summaries summaryProgress={noopProgress()} sidecar={readySidecar} />);
    expect(screen.queryByRole("button", { name: "停止" })).toBeNull();
    expect(screen.getByRole("button", { name: "生成" })).toBeTruthy();
  });
});
