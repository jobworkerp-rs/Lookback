import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ReflectionProgressHandle } from "@/hooks/useReflectionProgress";
import type { SidecarStatus } from "@/hooks/useSidecarStatus";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { ReflectionEntry } from "@/types/api";
import { ReflectionCard, Reflections } from "./Reflections";

const searchReflections = vi.fn();
const searchReflectionsHybrid = vi.fn();
const searchReflectionsByIntent = vi.fn();
const deleteReflection = vi.fn();
const enqueueReflectionJob = vi.fn();

vi.mock("@/api", () => ({
  searchReflections: (req: unknown) => searchReflections(req),
  searchReflectionsHybrid: (req: unknown) => searchReflectionsHybrid(req),
  searchReflectionsByIntent: (req: unknown) => searchReflectionsByIntent(req),
  deleteReflection: (id: unknown) => deleteReflection(id),
  enqueueReflectionJob: (req: unknown) => enqueueReflectionJob(req),
}));

function entry(over: Partial<ReflectionEntry> = {}): ReflectionEntry {
  return {
    id: "1",
    origin_thread_id: "10",
    summary: "",
    task_intent: "",
    task_category: 1,
    reflection_aspect: 1,
    outcome: 1,
    score: 0.5,
    score_self: 0.5,
    score_heuristic: 0.5,
    lessons: [],
    key_decisions: [],
    success_factors: [],
    failure_modes: [],
    mitigation_hint: null,
    pinned: false,
    prompt_version: "v1",
    intent_embedding_status: 0,
    created_at_ms: 0,
    updated_at_ms: 0,
    ...over,
  };
}

function renderCard(e: ReflectionEntry) {
  return renderWithProviders(
    <ReflectionCard entry={e} onOpenThread={() => {}} onDelete={() => {}} />,
  );
}

const reflectionProgress: ReflectionProgressHandle = {
  progress: null,
  start: vi.fn(),
  cancel: vi.fn(),
  clear: vi.fn(),
};

const readySidecar: SidecarStatus = { phase: "ready", warnings: [] };
const degradedSidecar: SidecarStatus = {
  phase: "ready",
  warnings: [{ kind: "vector_store_degraded", message: "degraded", detail: null }],
};

function renderReflections(sidecar: SidecarStatus = readySidecar) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <Reflections reflectionProgress={reflectionProgress} sidecar={sidecar} />
      </QueryClientProvider>
    </I18nextProvider>,
  );
}

beforeEach(() => {
  i18n.changeLanguage("ja");
  searchReflections.mockReset();
  searchReflectionsHybrid.mockReset();
  searchReflectionsByIntent.mockReset();
  deleteReflection.mockReset();
  enqueueReflectionJob.mockReset();
  searchReflections.mockResolvedValue([]);
  searchReflectionsHybrid.mockResolvedValue([]);
  searchReflectionsByIntent.mockResolvedValue([]);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("Reflections search", () => {
  it("loads the initial filter-only list through regular reflection search", async () => {
    renderReflections();

    await waitFor(() => expect(searchReflections).toHaveBeenCalled());
    expect(searchReflections.mock.calls[0]?.[0]).toEqual({
      outcomes: [],
      created_after_ms: undefined,
      limit: 200,
    });
    expect(searchReflectionsByIntent).not.toHaveBeenCalled();
  });

  it("searches reflections with memory hybrid search when query is present", async () => {
    searchReflections.mockResolvedValue([
      entry({
        id: "1",
        summary: "フレーキーなテストを修正した",
        lessons: ["CI の再試行条件を見直す"],
      }),
      entry({
        id: "2",
        summary: "設定画面の保存処理を整理した",
        key_decisions: ["dirty state を分離する"],
      }),
    ]);
    searchReflectionsHybrid.mockResolvedValue([
      entry({
        id: "1",
        summary: "フレーキーなテストを修正した",
        lessons: ["CI の再試行条件を見直す"],
      }),
    ]);
    renderReflections();
    await waitFor(() => expect(searchReflections).toHaveBeenCalledTimes(1));
    expect(await screen.findByText(/フレーキーなテスト/)).toBeInTheDocument();
    expect(screen.getByText(/設定画面/)).toBeInTheDocument();

    vi.useFakeTimers();
    fireEvent.change(screen.getByPlaceholderText(/自省を検索/), {
      target: { value: "  フレーキーなテスト  " },
    });

    act(() => vi.advanceTimersByTime(300));
    vi.useRealTimers();
    await waitFor(() => expect(screen.queryByText(/設定画面/)).toBeNull());
    expect(await screen.findByText(/フレーキーなテスト/)).toBeInTheDocument();
    expect(searchReflections).toHaveBeenCalledTimes(1);
    expect(searchReflectionsHybrid).toHaveBeenCalledWith({
      query_text: "フレーキーなテスト",
      outcomes: [],
      created_after_ms: undefined,
      limit: 50,
    });
    expect(searchReflectionsByIntent).not.toHaveBeenCalled();
  });

  it("falls back to local multi-word filtering when vector search is degraded", async () => {
    searchReflections.mockResolvedValue([
      entry({
        id: "1",
        summary: "フレーキーなテストを修正した",
        lessons: ["CI の再試行条件を見直す"],
      }),
      entry({
        id: "2",
        summary: "フレーキーな設定だけを整理した",
        lessons: ["保存処理を見直す"],
      }),
    ]);
    renderReflections(degradedSidecar);
    await waitFor(() => expect(searchReflections).toHaveBeenCalledTimes(1));
    expect(await screen.findByText(/フレーキーなテスト/)).toBeInTheDocument();
    expect(screen.getByText(/フレーキーな設定/)).toBeInTheDocument();

    vi.useFakeTimers();
    fireEvent.change(screen.getByPlaceholderText(/自省を検索/), {
      target: { value: "フレーキー CI" },
    });

    act(() => vi.advanceTimersByTime(300));
    vi.useRealTimers();
    await waitFor(() => expect(screen.queryByText(/フレーキーな設定/)).toBeNull());
    expect(screen.getByText(/フレーキーなテスト/)).toBeInTheDocument();
    expect(searchReflections).toHaveBeenCalledTimes(1);
    expect(searchReflectionsHybrid).not.toHaveBeenCalled();
  });

  it("debounces rapid query edits before searching with hybrid", async () => {
    searchReflections.mockResolvedValue([
      entry({ id: "1", summary: "フレーキーなテストを修正した" }),
      entry({ id: "2", summary: "設定画面の保存処理を整理した" }),
    ]);
    searchReflectionsHybrid.mockResolvedValue([
      entry({ id: "1", summary: "フレーキーなテストを修正した" }),
    ]);
    renderReflections();
    await waitFor(() => expect(searchReflections).toHaveBeenCalledTimes(1));
    expect(await screen.findByText(/設定画面/)).toBeInTheDocument();

    vi.useFakeTimers();
    const input = screen.getByPlaceholderText(/自省を検索/);
    fireEvent.change(input, { target: { value: "フ" } });
    fireEvent.change(input, { target: { value: "フレーキー" } });
    fireEvent.change(input, { target: { value: "フレーキーなテスト" } });

    act(() => vi.advanceTimersByTime(299));
    expect(searchReflections).toHaveBeenCalledTimes(1);
    expect(searchReflectionsHybrid).not.toHaveBeenCalled();
    expect(screen.getByText(/設定画面/)).toBeInTheDocument();

    act(() => vi.advanceTimersByTime(1));
    vi.useRealTimers();
    await waitFor(() => expect(screen.queryByText(/設定画面/)).toBeNull());
    expect(await screen.findByText(/フレーキーなテスト/)).toBeInTheDocument();
    expect(searchReflections).toHaveBeenCalledTimes(1);
    expect(searchReflectionsHybrid).toHaveBeenCalledTimes(1);
  });

  it("falls back to local filtering when hybrid search returns no hits", async () => {
    searchReflections.mockResolvedValue([
      entry({
        id: "1",
        summary: "フレーキーなテストを修正した",
        lessons: ["CI の再試行条件を見直す"],
      }),
      entry({
        id: "2",
        summary: "設定画面の保存処理を整理した",
        lessons: ["dirty state を分離する"],
      }),
    ]);
    searchReflectionsHybrid.mockResolvedValue([]);
    renderReflections();
    await waitFor(() => expect(searchReflections).toHaveBeenCalledTimes(1));
    expect(await screen.findByText(/フレーキーなテスト/)).toBeInTheDocument();

    vi.useFakeTimers();
    fireEvent.change(screen.getByPlaceholderText(/自省を検索/), {
      target: { value: "フレーキー" },
    });

    act(() => vi.advanceTimersByTime(300));
    vi.useRealTimers();
    await waitFor(() => expect(searchReflectionsHybrid).toHaveBeenCalledTimes(1));
    expect(await screen.findByText(/フレーキーなテスト/)).toBeInTheDocument();
    expect(screen.queryByText(/設定画面/)).toBeNull();
    expect(screen.queryByText("一致する自省がありません")).toBeNull();
  });

  it("shows the generated-empty copy when no filters are active", async () => {
    renderReflections();

    expect(await screen.findByText("自省がまだありません")).toBeInTheDocument();
  });

  it("shows the no-hit copy when search filters are active", async () => {
    renderReflections();
    await screen.findByText("自省がまだありません");

    vi.useFakeTimers();
    fireEvent.change(screen.getByPlaceholderText(/自省を検索/), {
      target: { value: "存在しない単語" },
    });

    act(() => vi.advanceTimersByTime(300));
    vi.useRealTimers();
    await waitFor(() => expect(searchReflectionsHybrid).toHaveBeenCalledTimes(1));
    expect(await screen.findByText("一致する自省がありません")).toBeInTheDocument();
  });
});

describe("ReflectionCard markdown rendering", () => {
  it("renders summary markdown as HTML (heading, bold, code)", () => {
    const { container } = renderCard(
      entry({ summary: "# 見出し\n\n本文に **強調** と `code` を含む" }),
    );
    expect(container.querySelector("h1")?.textContent).toBe("見出し");
    expect(container.querySelector("strong")?.textContent).toBe("強調");
    expect(container.querySelector("code")?.textContent).toBe("code");
    // The raw markdown markers must not survive as literal text.
    expect(screen.queryByText(/\*\*強調\*\*/)).toBeNull();
  });

  it("renders each lesson and key decision item as markdown", () => {
    const { container } = renderCard(
      entry({
        lessons: ["**早期に** テストを書く"],
        key_decisions: ["`rustls` を使う"],
      }),
    );
    const items = container.querySelectorAll(".reflection-section-list li");
    expect(items).toHaveLength(2);
    expect(items[0]?.querySelector("strong")?.textContent).toBe("早期に");
    expect(items[1]?.querySelector("code")?.textContent).toBe("rustls");
  });

  it("renders task_intent and mitigation_hint as markdown", () => {
    const { container } = renderCard(
      entry({ task_intent: "意図は **A**", mitigation_hint: "対策は `retry`" }),
    );
    expect(container.querySelector("strong")?.textContent).toBe("A");
    expect(container.querySelector("code")?.textContent).toBe("retry");
  });

  it("omits empty optional sections", () => {
    const { container } = renderCard(entry({ summary: "本文のみ" }));
    // No lessons / decisions / mitigation → no section blocks beyond the summary.
    expect(container.querySelectorAll(".reflection-section")).toHaveLength(0);
    expect(container.querySelector(".reflection-summary")?.textContent).toContain("本文のみ");
  });

  it("caps long lists at five items", () => {
    const { container } = renderCard(entry({ lessons: ["a", "b", "c", "d", "e", "f", "g"] }));
    expect(container.querySelectorAll(".reflection-section-list li")).toHaveLength(5);
  });
});
