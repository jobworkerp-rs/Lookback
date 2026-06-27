import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { SidecarStatus } from "@/hooks/useSidecarStatus";
import type { StepStreamProgressHandle } from "@/hooks/useStepStreamProgress";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { ResolvedSummaryMemoryRef, SummaryEntry } from "@/types/api";
import { Summaries } from "./Summaries";

// Mock the API surface that Summaries (and its descendants via
// `resolveSummaryRefNavigation` / `ThreadDetail`) hits during render.
vi.mock("@/api", () => ({
  listSummaries: vi.fn(),
  listSummaryPeriodKeys: vi.fn().mockResolvedValue([]),
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

import { listSummaries, resolveSummaryMemoryRef } from "@/api";

const mockListSummaries = vi.mocked(listSummaries);
const mockResolveSummaryMemoryRef = vi.mocked(resolveSummaryMemoryRef);

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
  mockResolveSummaryMemoryRef.mockReset();
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
