import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import {
  getReflectionSelectionContent,
  getSelectedMemoryLimits as getSelectedMemoryLimitsFromApi,
  getSummaryContent,
  listReflectionSelection,
  listSummaries,
  listSummariesForSelection,
} from "@/api";
import i18n from "@/i18n";
import { DEFAULT_SELECTED_MEMORY_LIMITS } from "@/lib/selectedMemory";
import { renderWithProviders } from "@/test-utils";
import type { ReflectionSelectionEntry, SummaryEntry } from "@/types/api";
import { SummarySelectionModal } from "./SummarySelectionModal";

vi.mock("@/api", () => ({
  getSelectedMemoryLimits: vi.fn(),
  getReflectionSelectionContent: vi.fn(),
  getSummaryContent: vi.fn(),
  listReflectionSelection: vi.fn(),
  listSummaries: vi.fn(),
  listSummariesForSelection: vi.fn(),
  parseSummaryContent: (summary: { content_json: string }) => {
    let parsed: Record<string, unknown> | null = null;
    try {
      const value: unknown = JSON.parse(summary.content_json);
      if (value && typeof value === "object" && !Array.isArray(value)) {
        parsed = value as Record<string, unknown>;
      }
    } catch {
      // Preserve legacy plain-text content as the raw fallback.
    }
    return { parsed, raw: summary.content_json };
  },
}));

const mockGetSummaryContent = vi.mocked(getSummaryContent);
const mockGetReflectionSelectionContent = vi.mocked(getReflectionSelectionContent);
const mockGetSelectedMemoryLimits = vi.mocked(getSelectedMemoryLimitsFromApi);
const mockListSummaries = vi.mocked(listSummaries);
const mockListReflectionSelection = vi.mocked(listReflectionSelection);
const mockListSummariesForSelection = vi.mocked(listSummariesForSelection);
const mockScrollIntoView = vi.fn();

beforeAll(() => {
  Element.prototype.scrollIntoView = mockScrollIntoView;
});

function entry(memory_id: string): SummaryEntry {
  return {
    memory_id,
    thread_id: "20",
    external_id: null,
    kind: "per-thread",
    period_key: null,
    scope_key: null,
    content_json: JSON.stringify({ body: `summary-${memory_id}` }),
    updated_at_ms: 1,
  };
}

function reflection(memory_id: string): ReflectionSelectionEntry {
  return {
    memory_id,
    origin_thread_id: "20",
    summary: `reflection-${memory_id}`,
    content_json: `reflection excerpt ${memory_id}`,
    source_thread_ids: ["20"],
    source_memory_ids: ["40"],
    created_at_ms: 1,
    updated_at_ms: 1,
  };
}

function reflectionPage(
  entries: ReflectionSelectionEntry[],
  next_cursor_after_memory_id: string | null = null,
) {
  return { entries, next_cursor_after_memory_id };
}

beforeEach(() => {
  i18n.changeLanguage("ja");
  mockScrollIntoView.mockReset();
  mockListSummaries.mockReset();
  mockListSummariesForSelection.mockReset();
  mockGetSummaryContent.mockReset();
  mockGetReflectionSelectionContent.mockReset();
  mockGetSelectedMemoryLimits.mockReset();
  mockListReflectionSelection.mockReset();
  mockGetSelectedMemoryLimits.mockResolvedValue(DEFAULT_SELECTED_MEMORY_LIMITS);
  mockGetSummaryContent.mockImplementation(async (memoryId) => entry(memoryId).content_json);
  mockGetReflectionSelectionContent.mockResolvedValue(null);
  mockListReflectionSelection.mockResolvedValue(reflectionPage([]));
  mockListSummariesForSelection.mockImplementation(async (req) => {
    const entries = await mockListSummaries(req);
    return {
      entries,
      next_offset: entries.length === 50 ? (req.offset ?? 0) + entries.length : null,
    };
  });
});

describe("SummarySelectionModal", () => {
  it("separates thread summaries from reflection in five tabs", async () => {
    mockListSummaries.mockResolvedValue([entry("summary-1")]);
    mockListReflectionSelection.mockResolvedValue(reflectionPage([reflection("900")]));
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );
    expect(screen.getAllByRole("tab")).toHaveLength(5);
    expect(screen.getByRole("tab", { name: "スレッド要約" })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: "自省" })).toBeInTheDocument();
    await screen.findByText("thread #20");
    fireEvent.click(screen.getByRole("tab", { name: "自省" }));
    await screen.findAllByText("reflection-900");
    expect(mockListReflectionSelection).toHaveBeenCalledWith({ limit: 50 });
    expect(screen.queryByText("thread #20")).not.toBeInTheDocument();
  });

  it("loads reflection pages with the last memory cursor and selects canonical content", async () => {
    mockListSummaries.mockResolvedValue([]);
    mockListReflectionSelection
      .mockResolvedValueOnce(
        reflectionPage(
          Array.from({ length: 50 }, (_, i) => reflection(String(i + 1))),
          "50",
        ),
      )
      .mockResolvedValueOnce(reflectionPage([reflection("99")]));
    mockGetReflectionSelectionContent.mockResolvedValue({
      memory_id: "1",
      origin_thread_id: "20",
      content_json: JSON.stringify({ summary: "全文", source_memory_ids: ["77"] }),
      source_thread_ids: ["20"],
      source_memory_ids: ["77"],
    });
    const onConfirm = vi.fn();
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={onConfirm} onClose={vi.fn()} />,
    );
    fireEvent.click(await screen.findByRole("tab", { name: "自省" }));
    await waitFor(() => expect(screen.getAllByRole("checkbox")).toHaveLength(50));
    fireEvent.click(screen.getByRole("button", { name: "さらに読み込む" }));
    await screen.findAllByText("reflection-99");
    expect(mockListReflectionSelection).toHaveBeenLastCalledWith({
      limit: 50,
      cursor_after_memory_id: "50",
    });
    const firstCheckbox = screen.getAllByRole("checkbox")[0] as HTMLInputElement;
    fireEvent.click(firstCheckbox);
    await waitFor(() => expect(firstCheckbox).toBeChecked());
    fireEvent.click(screen.getByRole("button", { name: "選択を確定" }));
    expect(onConfirm).toHaveBeenCalledWith([
      expect.objectContaining({
        kind: "reflection",
        source_memory_ids: ["77"],
        source_thread_ids: ["20"],
      }),
    ]);
  });

  it("drops a stale reflection load-more result after switching back to a summary tab", async () => {
    let resolveMore: ((entries: ReflectionSelectionEntry[]) => void) | undefined;
    mockListSummaries.mockResolvedValue([entry("summary-1")]);
    mockListReflectionSelection
      .mockResolvedValueOnce(
        reflectionPage(
          Array.from({ length: 50 }, (_, i) => reflection(String(i + 1))),
          "50",
        ),
      )
      .mockImplementationOnce(
        () =>
          new Promise<{
            entries: ReflectionSelectionEntry[];
            next_cursor_after_memory_id: string | null;
          }>((resolve) => {
            resolveMore = (entries) => resolve(reflectionPage(entries));
          }),
      );
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );
    fireEvent.click(await screen.findByRole("tab", { name: "自省" }));
    await waitFor(() => expect(screen.getAllByRole("checkbox")).toHaveLength(50));
    fireEvent.click(screen.getByRole("button", { name: "さらに読み込む" }));
    fireEvent.click(screen.getByRole("tab", { name: "スレッド要約" }));
    await screen.findByText("thread #20");
    resolveMore?.([reflection("stale-reflection")]);
    await waitFor(() =>
      expect(screen.queryByText("stale-reflection", { exact: false })).not.toBeInTheDocument(),
    );
  });

  it("renders reflection preview fields with human labels instead of enum numbers", async () => {
    mockListSummaries.mockResolvedValue([]);
    mockListReflectionSelection.mockResolvedValue(reflectionPage([reflection("900")]));
    mockGetReflectionSelectionContent.mockResolvedValue({
      memory_id: "900",
      origin_thread_id: "20",
      content_json: JSON.stringify({
        task_category: 1,
        reflection_aspect: 1,
        outcome: 1,
        score: 0.9,
        summary: "テストを修正した",
        task_intent: "壊れたテストを直す",
        lessons: ["再現手順を残す"],
        failure_modes: [3],
        facts: [{ note: "失敗ログを確認した", anchor_memory_id: "42" }],
      }),
      source_thread_ids: ["20"],
      source_memory_ids: ["42"],
    });
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );
    fireEvent.click(await screen.findByRole("tab", { name: "自省" }));
    fireEvent.click(await screen.findByRole("button", { name: "全文を確認" }));
    const preview = await screen.findByLabelText("全文を確認");
    expect(within(preview).getByText("コーディング")).toBeInTheDocument();
    expect(within(preview).getByText("成功")).toBeInTheDocument();
    expect(within(preview).getByText("スコープ逸脱")).toBeInTheDocument();
    expect(within(preview).getByText("再現手順を残す")).toBeInTheDocument();
    expect(within(preview).queryByText("failure_modes")).not.toBeInTheDocument();
  });

  it("loads a page and confirms the selected snapshot", async () => {
    mockListSummaries.mockResolvedValue([entry("1")]);
    const onConfirm = vi.fn();
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={onConfirm} onClose={vi.fn()} />,
    );
    await screen.findByText("thread #20");
    fireEvent.click(screen.getByRole("checkbox"));
    await waitFor(() => expect(screen.getByRole("checkbox")).toBeChecked());
    expect(mockGetSummaryContent).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "選択を確定" }));
    expect(onConfirm).toHaveBeenCalledWith(
      expect.arrayContaining([
        expect.objectContaining({ memory_id: "1", source_thread_ids: ["20"] }),
      ]),
    );
  });

  it("does not restore a cleared selection when its content request finishes", async () => {
    let resolveContent: ((content: string | null) => void) | undefined;
    mockListSummaries.mockResolvedValue([entry("1")]);
    mockGetSummaryContent.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveContent = resolve;
        }),
    );
    const onConfirm = vi.fn();
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={onConfirm} onClose={vi.fn()} />,
    );
    await screen.findByText("thread #20");

    fireEvent.click(screen.getByRole("checkbox"));
    fireEvent.click(screen.getByRole("button", { name: "選択を解除" }));
    resolveContent?.(entry("1").content_json);

    await waitFor(() => expect(screen.getByRole("checkbox")).not.toBeChecked());
    fireEvent.click(screen.getByRole("button", { name: "選択を確定" }));
    expect(onConfirm).toHaveBeenCalledWith([]);
  });

  it("offers retry after a page load failure", async () => {
    mockListSummaries.mockRejectedValueOnce(new Error("offline")).mockResolvedValueOnce([]);
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );
    await screen.findByText(/offline/);
    fireEvent.click(screen.getByRole("button", { name: "再試行" }));
    await waitFor(() => expect(mockListSummaries).toHaveBeenCalledTimes(2));
  });

  it("drops a stale load-more result after switching tabs", async () => {
    let resolveMore: ((entries: SummaryEntry[]) => void) | undefined;
    mockListSummaries
      .mockResolvedValueOnce(Array.from({ length: 50 }, (_, index) => entry(String(index + 1))))
      .mockImplementationOnce(
        () =>
          new Promise<SummaryEntry[]>((resolve) => {
            resolveMore = resolve;
          }),
      )
      .mockResolvedValueOnce([{ ...entry("daily-1"), kind: "daily", period_key: "2026-07-25" }]);
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );
    await waitFor(() => expect(screen.getAllByRole("checkbox")).toHaveLength(50));
    fireEvent.click(screen.getByRole("button", { name: "さらに読み込む" }));
    fireEvent.click(screen.getByRole("tab", { name: "日次" }));
    await screen.findByText("daily: 2026-07-25");
    resolveMore?.([entry("stale")]);
    await waitFor(() => expect(screen.queryByText(/summary-stale/)).not.toBeInTheDocument());
  });

  it("drops an old initial result after returning to the same summary tab", async () => {
    let resolveInitial: ((entries: SummaryEntry[]) => void) | undefined;
    mockListSummaries
      .mockImplementationOnce(
        () =>
          new Promise<SummaryEntry[]>((resolve) => {
            resolveInitial = resolve;
          }),
      )
      .mockResolvedValueOnce([{ ...entry("daily"), kind: "daily", period_key: "2026-08-09" }])
      .mockResolvedValueOnce([{ ...entry("fresh"), thread_id: "fresh" }]);
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );

    fireEvent.click(screen.getByRole("tab", { name: "日次" }));
    await screen.findByText("daily: 2026-08-09");
    fireEvent.click(screen.getByRole("tab", { name: "スレッド要約" }));
    await screen.findByText("thread #fresh");
    resolveInitial?.([{ ...entry("stale"), thread_id: "stale" }]);

    await waitFor(() => expect(screen.queryByText("thread #stale")).not.toBeInTheDocument());
  });

  it("resets loading-more and ignores an old load-more failure after switching tabs", async () => {
    let rejectMore: ((reason?: unknown) => void) | undefined;
    mockListSummaries
      .mockResolvedValueOnce(Array.from({ length: 50 }, (_, index) => entry(String(index + 1))))
      .mockImplementationOnce(
        () =>
          new Promise<SummaryEntry[]>((_, reject) => {
            rejectMore = reject;
          }),
      )
      .mockResolvedValueOnce(
        Array.from({ length: 50 }, (_, index) => ({
          ...entry(`daily-${index + 1}`),
          kind: "daily" as const,
          period_key: `2026-07-${index + 1}`,
        })),
      );
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );
    await waitFor(() => expect(screen.getAllByRole("checkbox")).toHaveLength(50));
    fireEvent.click(screen.getByRole("button", { name: "さらに読み込む" }));
    fireEvent.click(screen.getByRole("tab", { name: "日次" }));
    await screen.findByText("daily: 2026-07-1");
    expect(screen.getByRole("button", { name: "さらに読み込む" })).not.toBeDisabled();
    rejectMore?.(new Error("stale failure"));
    await waitFor(() => expect(screen.queryByText(/stale failure/)).not.toBeInTheDocument());
  });

  it("keeps the first ten selections and explains the selection limit", async () => {
    mockListSummaries.mockResolvedValue(
      Array.from({ length: 11 }, (_, index) => entry(String(index + 1))),
    );
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );
    await waitFor(() => expect(screen.getAllByRole("checkbox")).toHaveLength(11));
    for (const checkbox of screen.getAllByRole("checkbox")) fireEvent.click(checkbox);
    await waitFor(() =>
      expect(screen.getByText("一度に選択できる要約は10件までです。")).toBeInTheDocument(),
    );
    expect(screen.getByText("選択中: 10 / 10")).toBeInTheDocument();
  });

  it("shows the complete summary separately from the selection checkbox", async () => {
    const longEntry = {
      ...entry("1"),
      content_json: `${"前半".repeat(130)}-確認したい末尾-`,
    };
    mockListSummaries.mockResolvedValue([longEntry]);
    mockGetSummaryContent.mockResolvedValue(longEntry.content_json);
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );
    await screen.findByText("thread #20");
    expect(screen.queryByText("確認したい末尾", { exact: false })).not.toBeInTheDocument();
    fireEvent.click(screen.getByText("全文を確認"));
    await screen.findByText("確認したい末尾", { exact: false });
    expect(screen.getByRole("checkbox")).not.toBeChecked();
  });

  it("uses the list excerpt initially and lazily fetches content for previews and snapshots", async () => {
    const listed = { ...entry("1"), content_json: "list excerpt" };
    const fullContent = JSON.stringify({ body: "complete summary", source_memory_ids: ["42"] });
    mockListSummaries.mockResolvedValue([listed]);
    mockGetSummaryContent.mockResolvedValue(fullContent);
    const onConfirm = vi.fn();
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={onConfirm} onClose={vi.fn()} />,
    );
    await screen.findByText("thread #20");
    expect(mockGetSummaryContent).not.toHaveBeenCalled();
    expect(screen.getByText("list excerpt")).toBeInTheDocument();
    fireEvent.click(screen.getByText("全文を確認"));
    await waitFor(() =>
      expect(screen.getByLabelText("全文を確認")).toHaveTextContent("complete summary"),
    );
    expect(mockGetSummaryContent).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("checkbox"));
    await waitFor(() => expect(screen.getByRole("checkbox")).toBeChecked());
    expect(mockGetSummaryContent).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "選択を確定" }));
    expect(onConfirm).toHaveBeenCalledWith([
      expect.objectContaining({ content: fullContent, source_memory_ids: ["42"] }),
    ]);
  });

  it("does not select or preview a summary whose canonical content is unavailable", async () => {
    mockListSummaries.mockResolvedValue([entry("1")]);
    mockGetSummaryContent.mockResolvedValue(null);
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );

    await screen.findByText("thread #20");
    fireEvent.click(screen.getByRole("checkbox"));
    await screen.findByText(/summary content is unavailable/);
    expect(screen.getByRole("checkbox")).not.toBeChecked();

    fireEvent.click(screen.getByRole("button", { name: "全文を確認" }));
    await screen.findByText(/summary content is unavailable/);
    expect(screen.queryByLabelText("全文を確認")).not.toBeInTheDocument();
  });

  it("does not select or preview a reflection whose canonical content is unavailable", async () => {
    mockListSummaries.mockResolvedValue([]);
    mockListReflectionSelection.mockResolvedValue(reflectionPage([reflection("900")]));
    mockGetReflectionSelectionContent.mockResolvedValue(null);
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );

    fireEvent.click(await screen.findByRole("tab", { name: "自省" }));
    const checkbox = await screen.findByRole("checkbox");
    fireEvent.click(checkbox);
    await screen.findByText(/reflection content is unavailable/);
    expect(checkbox).not.toBeChecked();

    fireEvent.click(screen.getByRole("button", { name: "全文を確認" }));
    await screen.findByText(/reflection content is unavailable/);
    expect(screen.queryByLabelText("全文を確認")).not.toBeInTheDocument();
  });

  it("renders a fetched JSON summary as structured content", async () => {
    const listed = { ...entry("1"), content_json: "list excerpt" };
    const fullContent = JSON.stringify({
      title: "構造化された要約",
      summary: "## 目的\nMarkdown の本文です。",
      bullets: ["項目一", "項目二"],
      source_memory_ids: ["42", "43"],
      source_thread_ids: ["20"],
    });
    mockListSummaries.mockResolvedValue([listed]);
    mockGetSummaryContent.mockResolvedValue(fullContent);
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );

    await screen.findByText("thread #20");
    fireEvent.click(screen.getByText("全文を確認"));
    const preview = await screen.findByLabelText("全文を確認");

    await waitFor(() => expect(mockScrollIntoView).toHaveBeenCalledWith({ block: "nearest" }));
    expect(within(preview).getByText("構造化された要約")).toBeInTheDocument();
    expect(within(preview).getByText("要約")).toBeInTheDocument();
    expect(within(preview).getByText("Markdown の本文です。")).toBeInTheDocument();
    expect(within(preview).getByText("項目一")).toBeInTheDocument();
    expect(within(preview).getByText("項目二")).toBeInTheDocument();
    // Reference chips are intentionally static in this selection modal.
    expect(within(preview).getAllByRole("button")).toHaveLength(1);
    expect(preview.querySelector("pre")).toBeNull();

    const dialog = screen.getByRole("dialog");
    const scrollRegion = dialog.querySelector(".chat-summary-selection-body");
    expect(scrollRegion).not.toBeNull();
    expect(scrollRegion?.querySelector(".chat-summary-preview-body")).not.toBeNull();
    expect(scrollRegion?.querySelector(".modal-actions")).toBeNull();
    expect(dialog.querySelector(".modal-actions")).not.toBeNull();
  });

  it("renders non-JSON summary content as a readable fallback", async () => {
    const listed = { ...entry("1"), content_json: "list excerpt" };
    const fullContent = "旧形式の要約本文\n\nMarkdown のまま表示されます。";
    mockListSummaries.mockResolvedValue([listed]);
    mockGetSummaryContent.mockResolvedValue(fullContent);
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );

    await screen.findByText("thread #20");
    fireEvent.click(screen.getByText("全文を確認"));
    const preview = await screen.findByLabelText("全文を確認");

    expect(within(preview).getByText("旧形式の要約本文")).toBeInTheDocument();
    expect(within(preview).getByText("Markdown のまま表示されます。")).toBeInTheDocument();
    expect(preview.querySelector("pre")).toBeNull();
  });

  it("keeps list rows visible when content has not been selected", async () => {
    mockListSummaries.mockResolvedValue([entry("1"), entry("2")]);
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );
    await waitFor(() => expect(screen.getAllByRole("checkbox")).toHaveLength(2));
    expect(screen.getAllByRole("checkbox")).toHaveLength(2);
    expect(mockGetSummaryContent).not.toHaveBeenCalled();
  });

  it("keeps pagination aligned with unhydrated list rows", async () => {
    mockListSummaries
      .mockResolvedValueOnce(Array.from({ length: 50 }, (_, index) => entry(String(index + 1))))
      .mockResolvedValueOnce([]);
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );
    await waitFor(() => expect(screen.getAllByRole("checkbox")).toHaveLength(50));
    expect(mockGetSummaryContent).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "さらに読み込む" }));
    await waitFor(() =>
      expect(mockListSummaries).toHaveBeenLastCalledWith({
        kind: "per-thread",
        limit: 50,
        offset: 50,
      }),
    );
  });

  it("uses the dedicated summary page continuation even when a page has few valid rows", async () => {
    mockListSummariesForSelection
      .mockResolvedValueOnce({ entries: [entry("1")], next_offset: 50 })
      .mockResolvedValueOnce({ entries: [entry("51")], next_offset: null });
    renderWithProviders(
      <SummarySelectionModal selected={[]} onConfirm={vi.fn()} onClose={vi.fn()} />,
    );
    await screen.findByText("thread #20");
    fireEvent.click(screen.getByRole("button", { name: "さらに読み込む" }));
    await waitFor(() =>
      expect(mockListSummariesForSelection).toHaveBeenLastCalledWith({
        kind: "per-thread",
        limit: 50,
        offset: 50,
      }),
    );
    expect(screen.getAllByRole("checkbox")).toHaveLength(2);
  });
});
