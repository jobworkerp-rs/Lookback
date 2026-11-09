import { fireEvent, screen, waitFor } from "@testing-library/react";
import { useState } from "react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { SidecarStatus } from "@/hooks/useSidecarStatus";
import { type StagedProgressHandle, SUMMARY_GENERATION_STEPS } from "@/hooks/useStagedProgress";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { ResolvedSummaryMemoryRef, StagedProgressSnapshot, SummaryEntry } from "@/types/api";

const { incrementUiMemoryCounterMock } = vi.hoisted(() => ({
  incrementUiMemoryCounterMock: vi.fn(),
}));

import { periodKeyPrefixesForMonth, Summaries } from "./Summaries";

// Mock the API surface that Summaries (and its descendants via
// `resolveSummaryRefNavigation` / `ThreadDetail`) hits during render.
vi.mock("@/api", () => ({
  listSummaries: vi.fn(),
  listSummaryPathChildren: vi.fn(),
  listSummaryPeriodKeys: vi.fn().mockResolvedValue([]),
  getSummaryRecoveryStatus: vi.fn().mockResolvedValue({
    health: "healthy",
    record_count: 0,
    detail: null,
    records: [],
  }),
  deleteSummary: vi.fn(),
  enqueueThreadGroupSummaryJob: vi.fn(),
  generateSummaries: vi.fn(),
  getSummaryContent: vi.fn(),
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
vi.mock("@/lib/uiMemoryDiagnostics", () => ({
  incrementUiMemoryCounter: incrementUiMemoryCounterMock,
}));

import {
  enqueueThreadGroupSummaryJob,
  generateSummaries,
  getSummaryContent,
  getSummaryRecoveryStatus,
  listSummaries,
  listSummaryPathChildren,
  listSummaryPeriodKeys,
  resolveSummaryMemoryRef,
  searchMemoriesKeyword,
} from "@/api";

const mockListSummaries = vi.mocked(listSummaries);
const mockListSummaryPathChildren = vi.mocked(listSummaryPathChildren);
const mockGetSummaryContent = vi.mocked(getSummaryContent);
const mockListSummaryPeriodKeys = vi.mocked(listSummaryPeriodKeys);
const mockResolveSummaryMemoryRef = vi.mocked(resolveSummaryMemoryRef);
const mockSearchMemoriesKeyword = vi.mocked(searchMemoriesKeyword);
const mockGetSummaryRecoveryStatus = vi.mocked(getSummaryRecoveryStatus);
const mockEnqueueThreadGroupSummaryJob = vi.mocked(enqueueThreadGroupSummaryJob);
const mockGenerateSummaries = vi.mocked(generateSummaries);

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
  localStorage.clear();
  i18n.changeLanguage("ja");
  mockListSummaries.mockReset();
  mockListSummaryPathChildren.mockReset();
  mockGetSummaryContent.mockReset();
  mockListSummaryPeriodKeys.mockReset();
  mockResolveSummaryMemoryRef.mockReset();
  mockSearchMemoriesKeyword.mockReset();
  mockEnqueueThreadGroupSummaryJob.mockReset();
  mockGenerateSummaries.mockReset().mockResolvedValue({ job_id_hint: "summary-job" });
  mockListSummaryPeriodKeys.mockResolvedValue([]);
  mockSearchMemoriesKeyword.mockResolvedValue([]);
  mockGetSummaryRecoveryStatus.mockResolvedValue({
    health: "healthy",
    record_count: 0,
    detail: null,
    records: [],
  });
  incrementUiMemoryCounterMock.mockClear();
});

function noopProgress(): StagedProgressHandle<"summary-generation"> {
  return {
    snapshot: null,
    busy: false,
    reset: vi.fn(),
    clear: vi.fn(),
    dismissToast: vi.fn(),
    toastVisible: false,
    cancel: vi.fn().mockResolvedValue(undefined),
  };
}

function busyProgress(
  overrides: Partial<StagedProgressHandle<"summary-generation">> = {},
): StagedProgressHandle<"summary-generation"> {
  const steps = Object.fromEntries(
    SUMMARY_GENERATION_STEPS.map((step) => [step, { status: "waiting", message: null }]),
  ) as StagedProgressSnapshot<"summary-generation">["steps"];
  steps["thread-summary"] = { status: "active", message: "(1/3) スレッド要約" };
  return {
    snapshot: {
      kind: "summary-generation",
      job_id: "summary-1",
      sequence: 1,
      steps,
    },
    busy: true,
    reset: vi.fn(),
    clear: vi.fn(),
    dismissToast: vi.fn(),
    toastVisible: false,
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
    issue_tracker_port: null,
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

function threadGroupEntry(over: Partial<SummaryEntry> = {}): SummaryEntry {
  return {
    ...perThreadEntry({
      memory_id: "1999",
      thread_id: null,
      group_id: "42",
      external_id: "thread-group-summary:42",
      kind: "thread-group",
    }),
    ...over,
  };
}

function dailyOverallEntry(over: Partial<SummaryEntry> = {}): SummaryEntry {
  return {
    ...perThreadEntry({
      memory_id: "1001",
      thread_id: null,
      external_id: "daily:v2:1:2026-07-10:scope:overall",
      kind: "daily",
      period_key: "2026-07-10",
      scope_key: "scope",
      rollup_scope: "overall",
      source_scope_digest: "scope",
      path_summary_count: 2,
      completion_status: "incomplete",
      path_bucket_diagnostics: [
        {
          path_bucket_digest: "missing-path",
          statuses: ["failed", "missing"],
          path_bucket_kind: "label",
          path_label: "path:/workspace/example/project",
        },
      ],
    }),
    ...over,
  };
}

function renderSummaries() {
  return renderWithProviders(
    <Summaries summaryGenerationProgress={noopProgress()} sidecar={readySidecar} />,
  );
}

function renderCalendar(kind: "daily" | "weekly" | "monthly", month = "2026-07") {
  return renderWithProviders(
    <Summaries
      summaryGenerationProgress={noopProgress()}
      sidecar={readySidecar}
      focus={{ kind, month, periodKey: "" }}
    />,
  );
}

describe("Summaries calendar", () => {
  it("remembers a period card bulk display choice only when enabled", async () => {
    mockListSummaries.mockResolvedValue([
      dailyOverallEntry({
        content_json: JSON.stringify({
          overall_purpose: "概要",
          purpose_groups: [{ purpose: "確認", bullets: ["本文"] }],
        }),
      }),
    ]);
    renderSummaries();

    fireEvent.click(screen.getByRole("button", { name: "日次" }));
    const remember = await screen.findByRole("checkbox", { name: /初期表示を記憶する/ });
    expect(remember).not.toBeChecked();
    fireEvent.click(remember);
    fireEvent.click(screen.getByRole("button", { name: "すべて展開" }));

    expect(localStorage.getItem("lookback.periodSummaryDisplay")).toBe(
      '{"remember":true,"mode":"expanded"}',
    );
  });

  it("uses the remembered bulk mode when a period summary is rendered again", async () => {
    localStorage.setItem("lookback.periodSummaryDisplay", '{"remember":true,"mode":"expanded"}');
    mockListSummaries.mockResolvedValue([
      dailyOverallEntry({
        content_json: JSON.stringify({
          purpose_groups: [{ purpose: "確認", bullets: ["本文"] }],
        }),
      }),
    ]);
    renderSummaries();

    fireEvent.click(screen.getByRole("button", { name: "日次" }));
    await screen.findByText("確認");
    expect(document.querySelectorAll(".summary-structured-card[open]")).toHaveLength(1);
  });

  it("shows persisted path diagnostics even when that child has no summary row", async () => {
    mockListSummaries.mockResolvedValue([dailyOverallEntry()]);
    renderSummaries();

    fireEvent.click(screen.getByRole("button", { name: "日次" }));
    await waitFor(() => {
      expect(mockListSummaries).toHaveBeenCalledWith(expect.objectContaining({ kind: "daily" }));
    });
    const details = await screen.findByText("欠落・再利用した path の詳細");
    fireEvent.click(details);

    expect(await screen.findByText(/\/workspace\/example\/project: failed, missing/)).toBeTruthy();
  });

  it("keeps a legacy-only body as a collapsed previous representation beside incomplete v2", async () => {
    const legacy = dailyOverallEntry({
      memory_id: "1002",
      external_id: "daily:1:2026-07-10:_all",
      scope_key: "_all",
      rollup_scope: null,
      source_scope_digest: null,
      completion_status: null,
      legacy: true,
      path_bucket_diagnostics: [],
    });
    mockListSummaries.mockResolvedValue([dailyOverallEntry(), legacy]);
    renderSummaries();

    fireEvent.click(screen.getByRole("button", { name: "日次" }));
    const previous = await screen.findByText("以前の形式の要約 (1)");
    expect(previous).toBeTruthy();
    expect(previous.parentElement?.tagName).toBe("DETAILS");
    expect(previous.parentElement).not.toHaveAttribute("open");
    fireEvent.click(previous);
    expect(previous.parentElement).toHaveAttribute("open");
  });

  it("marks a failed newer generation without hiding the saved body", async () => {
    mockListSummaries.mockResolvedValue([
      dailyOverallEntry({
        generation_id: "saved-generation",
        last_material_attempt_generation_id: "failed-generation",
        last_material_attempt_status: "failed",
      }),
    ]);
    renderSummaries();

    fireEvent.click(screen.getByRole("button", { name: "日次" }));
    expect(await screen.findByText("新しい生成は失敗（前回本文を表示中）")).toBeTruthy();
    expect(screen.getByText("Coding session")).toBeTruthy();
  });

  it("shows the number of sources omitted by a context limit", async () => {
    mockListSummaries.mockResolvedValue([
      dailyOverallEntry({ context_truncated: true, omitted_source_count: 2 }),
    ]);
    renderSummaries();

    fireEvent.click(screen.getByRole("button", { name: "日次" }));
    expect(await screen.findByText("context 上限により 2 件を含められませんでした")).toBeTruthy();
  });

  it("opens a newly supplied focus in its selected calendar period", async () => {
    function FocusHarness({ onFocusConsumed }: { onFocusConsumed: () => void }) {
      const [focus, setFocus] = useState<{
        kind: "daily";
        month: string;
        periodKey: string;
      } | null>(null);
      return (
        <>
          <button
            type="button"
            onClick={() => setFocus({ kind: "daily", month: "2026-07", periodKey: "2026-07-10" })}
          >
            フォーカスを設定
          </button>
          <Summaries
            summaryGenerationProgress={noopProgress()}
            sidecar={readySidecar}
            focus={focus}
            onFocusConsumed={onFocusConsumed}
          />
        </>
      );
    }

    mockListSummaries.mockResolvedValue([]);
    mockListSummaryPeriodKeys.mockResolvedValue(["2026-07-10"]);
    const onFocusConsumed = vi.fn();
    renderWithProviders(<FocusHarness onFocusConsumed={onFocusConsumed} />);
    fireEvent.click(screen.getByRole("button", { name: "フォーカスを設定" }));

    await waitFor(() => {
      expect(mockListSummaryPeriodKeys).toHaveBeenCalledWith({
        kind: "daily",
        period_key_prefixes: ["2026-07-"],
      });
    });
    expect(screen.getByRole("button", { name: "10" })).toHaveClass("selected");
    expect(onFocusConsumed).toHaveBeenCalledTimes(1);
  });

  it("opens the daily calendar when selected from per-thread summaries", async () => {
    mockListSummaries.mockResolvedValue([]);
    renderSummaries();

    const calendarButton = screen.getByRole("button", { name: "カレンダー" });
    expect(calendarButton).toBeEnabled();
    fireEvent.click(calendarButton);

    await waitFor(() => {
      expect(mockListSummaryPeriodKeys).toHaveBeenCalledWith(
        expect.objectContaining({ kind: "daily" }),
      );
    });
  });

  it("restores the last period calendar month and selected key after visiting per-thread", async () => {
    mockListSummaryPeriodKeys.mockResolvedValue(["2026-W27"]);
    mockListSummaries.mockResolvedValue([
      perThreadEntry({ kind: "weekly", period_key: "2026-W27" }),
    ]);
    renderCalendar("weekly", "2026-07");

    const selectedWeek = (await screen.findAllByTitle("2026-W27"))[0];
    fireEvent.click(selectedWeek as HTMLElement);
    await waitFor(() => {
      expect(mockListSummaries).toHaveBeenCalledWith(
        expect.objectContaining({ kind: "weekly", period_key: "2026-W27" }),
      );
    });

    fireEvent.click(screen.getByRole("button", { name: "スレッドグループ" }));
    fireEvent.click(screen.getByRole("button", { name: "カレンダー" }));

    await waitFor(() => {
      expect(mockListSummaryPeriodKeys).toHaveBeenLastCalledWith({
        kind: "weekly",
        period_key_prefixes: ["2026-W27", "2026-W28", "2026-W29", "2026-W30", "2026-W31"],
      });
    });
    expect(screen.getAllByTitle("2026-W27")[0]).toHaveClass("selected");
  });

  it("clears the selected key when switching directly between period kinds", async () => {
    mockListSummaryPeriodKeys.mockResolvedValue(["2026-W27"]);
    mockListSummaries.mockResolvedValue([
      perThreadEntry({ kind: "weekly", period_key: "2026-W27" }),
    ]);
    renderCalendar("weekly", "2026-07");

    fireEvent.click((await screen.findAllByTitle("2026-W27"))[0] as HTMLElement);
    await waitFor(() => {
      expect(mockListSummaries).toHaveBeenCalledWith(
        expect.objectContaining({ kind: "weekly", period_key: "2026-W27" }),
      );
    });

    fireEvent.click(screen.getByRole("button", { name: "日次" }));
    fireEvent.click(screen.getByRole("button", { name: "週次" }));

    await waitFor(() => {
      expect(mockListSummaryPeriodKeys).toHaveBeenLastCalledWith({
        kind: "weekly",
        period_key_prefixes: ["2026-W27", "2026-W28", "2026-W29", "2026-W30", "2026-W31"],
      });
    });
    expect(screen.getAllByTitle("2026-W27")[0]).not.toHaveClass("selected");
  });

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

describe("Summaries public summary filter", () => {
  it("counts summary list and card render attempts without refetching the list", async () => {
    mockListSummaries.mockResolvedValue([threadGroupEntry()]);
    renderSummaries();

    expect(await screen.findByText("Coding session")).toBeTruthy();
    expect(incrementUiMemoryCounterMock).toHaveBeenCalledWith("summary_list_render_attempts_total");
    expect(incrementUiMemoryCounterMock).toHaveBeenCalledWith("summary_card_render_attempts_total");
    expect(mockListSummaries).toHaveBeenCalledTimes(1);
  });

  it("starts with thread-group summaries and does not expose per-thread filters", async () => {
    mockListSummaries.mockResolvedValue([]);
    renderSummaries();

    await waitFor(() => {
      expect(mockListSummaries).toHaveBeenLastCalledWith(
        expect.objectContaining({ kind: "thread-group" }),
      );
    });
    expect(screen.getByRole("button", { name: "スレッドグループ" })).toHaveClass("active");
    expect(screen.queryByText(/^ラベルで絞り込む/)).toBeNull();
    expect(screen.queryByRole("button", { name: "スレッド" })).toBeNull();
  });

  it("does not render a per-thread row returned by a stale list response", async () => {
    mockListSummaries.mockResolvedValue([perThreadEntry()]);
    renderSummaries();

    await screen.findByText("要約はまだ生成されていません");
    expect(screen.queryByText("Coding session")).not.toBeInTheDocument();
  });

  it("counts only public rows when a stale per-thread row is returned", async () => {
    mockListSummaries.mockResolvedValue([perThreadEntry(), threadGroupEntry()]);
    renderSummaries();

    expect(await screen.findByText("1 件")).toBeInTheDocument();
    expect(screen.queryByText("2 件")).not.toBeInTheDocument();
  });

  it("searches thread-group summaries by the public memory kind and label", async () => {
    mockListSummaries.mockResolvedValue([]);
    renderSummaries();

    fireEvent.click(screen.getByRole("button", { name: "検索" }));
    fireEvent.click(screen.getByRole("button", { name: "Keyword" }));
    fireEvent.change(screen.getByPlaceholderText("検索クエリ"), { target: { value: "review" } });

    await waitFor(() => {
      expect(mockSearchMemoriesKeyword).toHaveBeenLastCalledWith(
        expect.objectContaining({
          user_id: 1,
          memory_kinds: [8],
          labels_any: ["thread_group_summary"],
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
});

describe("Summaries thread-group navigation", () => {
  it("renders a group summary as a button and opens the group detail on click", async () => {
    mockListSummaries.mockResolvedValue([threadGroupEntry()]);
    const onNavigateThreadGroup = vi.fn();
    renderWithProviders(
      <Summaries
        summaryGenerationProgress={noopProgress()}
        sidecar={readySidecar}
        onNavigateThreadGroup={onNavigateThreadGroup}
      />,
    );
    const link = await screen.findByRole("button", { name: "グループ #42" });
    fireEvent.click(link);
    expect(onNavigateThreadGroup).toHaveBeenCalledWith("42");
  });
});

describe("Summaries source_memory_ids chip", () => {
  it("opens the cited period summary in its calendar slot", async () => {
    mockListSummaries.mockResolvedValue([threadGroupEntry()]);
    mockListSummaryPeriodKeys.mockResolvedValue(["2026-07-10"]);
    mockResolveSummaryMemoryRef.mockResolvedValue({
      memory_id: "111",
      thread_id: null,
      external_id: "daily_summary:2026-07-10",
      kind: "daily",
      period_key: "2026-07-10",
      scope_key: "_all",
    });
    renderSummaries();

    fireEvent.click((await screen.findAllByTitle(/^memory /))[0] as HTMLElement);

    await waitFor(() => {
      expect(mockListSummaryPeriodKeys).toHaveBeenCalledWith({
        kind: "daily",
        period_key_prefixes: ["2026-07-"],
      });
    });
    expect(screen.getByRole("button", { name: "10" })).toHaveClass("selected");
  });

  it("opens the owning thread group when the resolved memory is a group summary", async () => {
    mockListSummaries.mockResolvedValue([threadGroupEntry()]);
    const resolved: ResolvedSummaryMemoryRef = {
      memory_id: "111",
      thread_id: null,
      group_id: "555",
      external_id: "thread-group-summary:555",
      kind: "thread-group",
      period_key: null,
      scope_key: null,
    };
    mockResolveSummaryMemoryRef.mockResolvedValue(resolved);
    const onNavigateThreadGroup = vi.fn();
    renderWithProviders(
      <Summaries
        summaryGenerationProgress={noopProgress()}
        sidecar={readySidecar}
        onNavigateThreadGroup={onNavigateThreadGroup}
      />,
    );
    // Two source_memory_ids in the content → two chips (numbered 1 / 2).
    const chips = await screen.findAllByTitle(/^memory /);
    expect(chips).toHaveLength(2);
    fireEvent.click(chips[0] as HTMLElement);
    await waitFor(() => {
      expect(mockResolveSummaryMemoryRef).toHaveBeenCalledWith("111");
    });
    expect(onNavigateThreadGroup).toHaveBeenCalledWith("555");
  });

  it("leaves the modal closed when the cited memory cannot be resolved", async () => {
    mockListSummaries.mockResolvedValue([threadGroupEntry()]);
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
  it("keeps retained recovery periods behind the toolbar details dialog", async () => {
    mockListSummaries.mockResolvedValue([]);
    mockGetSummaryRecoveryStatus.mockResolvedValue({
      health: "healthy",
      record_count: 1,
      detail: null,
      records: [
        {
          memories_endpoint_id: "local",
          owner_id: 1,
          source_scope_digest: "scope",
          period_kind: "daily",
          period_key: "2026-09-03",
          rollup_scope: "path",
          path_bucket_digest: "bucket",
          path_bucket_kind: "label",
          path_label: "path:/workspace/example/project",
          stage: "path_summary",
          reason_code: "llm_execution_failed",
          latest_generation_id: "generation",
          first_failed_at: "2026-09-03T00:00:00Z",
          last_attempt_at: "2026-09-03T00:00:01Z",
          last_trigger_kind: "manual",
          detail: "timeout",
        },
      ],
    });
    renderSummaries();
    expect(screen.queryByText(/daily: 2026-09-03/)).toBeNull();

    fireEvent.click(await screen.findByRole("button", { name: "未完了の要約 1件の詳細を表示" }));
    expect(await screen.findByRole("dialog", { name: "未完了の要約" })).toBeTruthy();
    expect(screen.getByText(/daily: 2026-09-03/)).toBeTruthy();
    expect(screen.getByText(/\/workspace\/example\/project/)).toBeTruthy();
    expect(screen.getByText("timeout")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "閉じる" }));
    expect(screen.queryByRole("dialog", { name: "未完了の要約" })).toBeNull();
  });

  it("hides the recovery details control when no records are retained", async () => {
    mockListSummaries.mockResolvedValue([]);
    renderSummaries();
    expect(await screen.findByText("要約はまだ生成されていません")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /未完了の要約.*詳細/ })).toBeNull();
  });

  it("keeps an unhealthy recovery state prominent without offering the details dialog", async () => {
    mockListSummaries.mockResolvedValue([]);
    mockGetSummaryRecoveryStatus.mockResolvedValue({
      health: "corrupt",
      record_count: 0,
      detail: "invalid recovery state",
      records: [],
    });
    renderSummaries();
    expect(await screen.findByRole("alert")).toHaveTextContent("要約の回復状態を確認できない");
    expect(screen.queryByRole("button", { name: /未完了の要約.*詳細/ })).toBeNull();
  });

  it("visually groups Path summaries and promotes each path to a heading", async () => {
    mockListSummaries.mockResolvedValue([dailyOverallEntry()]);
    mockListSummaryPathChildren.mockResolvedValue([
      {
        memory_id: "path-1",
        path_bucket: {
          path_bucket_digest: "path-1",
          path_bucket_kind: "label",
          path_label: "path:/workspace/example/src/api",
        },
      },
    ]);
    mockGetSummaryContent.mockResolvedValue(JSON.stringify({ summary: "API を更新した。" }));
    renderSummaries();

    const disclosure = await screen.findByText(/path ごとの要約/);
    fireEvent.click(disclosure);
    const section = disclosure.closest("details");
    expect(section).toHaveClass("summary-path-section");
    expect(await screen.findByText("この期間の内容を path 単位で分けて確認できます")).toBeTruthy();

    const path = await screen.findByText("/workspace/example/src/api");
    fireEvent.click(path);
    expect(path.closest("details")).toHaveClass("summary-path-row");
    expect(await screen.findByText("API を更新した。")).toBeTruthy();
  });

  it("hides a one-child Path breakdown while retaining the overall summary", async () => {
    mockListSummaries.mockResolvedValue([dailyOverallEntry({ path_summary_count: 1 })]);
    renderSummaries();
    await screen.findByText("2026-07-10");
    expect(screen.queryByText(/path ごとの要約/)).toBeNull();
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
    renderWithProviders(<Summaries summaryGenerationProgress={handle} sidecar={readySidecar} />);
    expect(screen.queryByRole("button", { name: "生成" })).toBeNull();
    expect(screen.getByRole("button", { name: "停止" })).toBeTruthy();
  });

  it("renders each summary stage row alongside the aggregate running state", async () => {
    mockListSummaries.mockResolvedValue([]);
    renderWithProviders(
      <Summaries summaryGenerationProgress={busyProgress()} sidecar={readySidecar} />,
    );

    expect(screen.getByText("生成中")).toBeInTheDocument();
    expect(screen.getByText("スレッド要約").parentElement).toHaveTextContent("(1/3) スレッド要約");
    expect(screen.getByText("スレッドグループ要約").parentElement).toHaveTextContent("待機中");
    expect(
      screen.getAllByText("日次").some((element) => element.closest(".staged-progress-row")),
    ).toBe(true);
    expect(
      screen.getAllByText("週次").some((element) => element.closest(".staged-progress-row")),
    ).toBe(true);
    expect(
      screen.getAllByText("月次").some((element) => element.closest(".staged-progress-row")),
    ).toBe(true);
  });

  it("invokes summaryGenerationProgress.cancel when the user clicks 停止", async () => {
    mockListSummaries.mockResolvedValue([]);
    const cancel = vi.fn().mockResolvedValue(undefined);
    const handle = busyProgress({ cancel });
    renderWithProviders(<Summaries summaryGenerationProgress={handle} sidecar={readySidecar} />);
    fireEvent.click(screen.getByRole("button", { name: "停止" }));
    await waitFor(() => expect(cancel).toHaveBeenCalledTimes(1));
  });

  it("restores 生成 once the dispatch settles", async () => {
    mockListSummaries.mockResolvedValue([]);
    renderWithProviders(
      <Summaries summaryGenerationProgress={noopProgress()} sidecar={readySidecar} />,
    );
    expect(screen.queryByRole("button", { name: "停止" })).toBeNull();
    expect(screen.getByRole("button", { name: "生成" })).toBeTruthy();
  });
});

describe("Summaries generate dialog", () => {
  it.each(["日次", "週次", "月次"])(
    "opens the dialog from the %s tab before dispatching",
    async (kind) => {
      mockListSummaries.mockResolvedValue([]);
      renderSummaries();

      fireEvent.click(screen.getByRole("button", { name: kind }));
      fireEvent.click(screen.getByRole("button", { name: "生成" }));

      expect(await screen.findByRole("dialog", { name: "要約を生成" })).toBeInTheDocument();
      expect(mockGenerateSummaries).not.toHaveBeenCalled();
    },
  );

  it("opens the stage and period dialog from the ThreadGroup tab before dispatching", async () => {
    mockListSummaries.mockResolvedValue([]);
    renderSummaries();

    fireEvent.click(screen.getByRole("button", { name: "生成" }));

    expect(await screen.findByRole("dialog", { name: "要約を生成" })).toBeInTheDocument();
    expect(mockEnqueueThreadGroupSummaryJob).not.toHaveBeenCalled();
    expect(mockGenerateSummaries).not.toHaveBeenCalled();
    expect(screen.getByRole("checkbox", { name: "スレッドグループ" })).toBeChecked();
    expect(screen.getByRole("checkbox", { name: "日次" })).not.toBeChecked();
    expect(screen.getByRole("button", { name: "生成を開始" })).toBeDisabled();

    const today = String(new Date().getDate());
    fireEvent.click(screen.getByLabelText("開始"));
    fireEvent.click(screen.getByRole("button", { name: today }));
    fireEvent.click(screen.getByLabelText("終了"));
    fireEvent.click(screen.getByRole("button", { name: today }));
    fireEvent.click(screen.getByRole("button", { name: "生成を開始" }));

    await waitFor(() =>
      expect(mockGenerateSummaries).toHaveBeenCalledWith(
        expect.objectContaining({
          run_per_thread: true,
          run_thread_group: true,
          run_daily: false,
          thread_group_activity_after_ms: expect.any(Number),
          thread_group_activity_before_ms: expect.any(Number),
        }),
      ),
    );
  });
});
