import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { SidecarStatus } from "@/hooks/useSidecarStatus";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type {
  LabelWithCount,
  ListThreadsRequest,
  SearchThreadsRequest,
  ThreadSummary,
} from "@/types/api";
import { Threads } from "./Threads";

// A healthy (non-degraded) sidecar so embedding search modes stay enabled.
const readySidecar: SidecarStatus = { phase: "ready", warnings: [] };

const listThreads = vi.fn();
const findDistinctLabels = vi.fn();
const findCoOccurringLabels = vi.fn();
const deleteThread = vi.fn();
const searchMemoriesKeyword = vi.fn();
const searchMemoriesSemantic = vi.fn();
const searchMemoriesHybrid = vi.fn();

vi.mock("@/api", () => ({
  listThreads: (req: unknown) => listThreads(req),
  findDistinctLabels: (req: unknown) => findDistinctLabels(req),
  findCoOccurringLabels: (req: unknown) => findCoOccurringLabels(req),
  deleteThread: (id: unknown) => deleteThread(id),
  searchMemoriesKeyword: (req: unknown) => searchMemoriesKeyword(req),
  searchMemoriesSemantic: (req: unknown) => searchMemoriesSemantic(req),
  searchMemoriesHybrid: (req: unknown) => searchMemoriesHybrid(req),
}));

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

function thread(id: string, labels: string[], description = `Thread ${id}`): ThreadSummary {
  return {
    id,
    user_id: "1",
    description,
    channel: "codex",
    labels,
    created_at_ms: 1700000000000,
    updated_at_ms: 1700000000000,
  };
}

const labels: LabelWithCount[] = [
  { label: "lookback", thread_count: 5 },
  { label: "review", thread_count: 3 },
  { label: "codex", thread_count: 8 },
];

describe("Threads page label filter", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    listThreads.mockReset();
    findDistinctLabels.mockReset();
    findCoOccurringLabels.mockReset();
    searchMemoriesKeyword.mockReset();
    searchMemoriesSemantic.mockReset();
    searchMemoriesHybrid.mockReset();
    // Default safe response so the co-occurring query doesn't crash when
    // a test triggers AND but doesn't explicitly mock the call.
    findCoOccurringLabels.mockResolvedValue([]);
  });

  // Helper: pick the LabelFilter bar chip (which suffixes the count, eg "lookback (5)")
  // — avoids ambiguity with the in-card `.label-pill` whose accessible name is bare.
  const barChip = (label: string, count: number) =>
    screen.getByRole("button", { name: `${label} (${count})` });

  // The bar starts collapsed when no labels are selected; open it so chip
  // queries inside the test can find buttons.
  const openLabelBar = async () => {
    const summary = await screen.findByText(/^ラベルで絞り込む/);
    fireEvent.click(summary);
  };

  it("passes selected labels (and label_match when 2+) into listThreads", async () => {
    listThreads.mockResolvedValue([thread("1", [])]);
    findDistinctLabels.mockResolvedValue(labels);

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={readySidecar} />);

    await openLabelBar();
    await waitFor(() => barChip("lookback", 5));
    expect(findDistinctLabels).toHaveBeenCalledWith({ user_id: 1, limit: 10_000 });
    // First load: no label filter.
    await waitFor(() => expect(listThreads).toHaveBeenCalled());
    expect((listThreads.mock.calls[0]?.[0] as ListThreadsRequest).labels_any).toBeUndefined();

    fireEvent.click(barChip("lookback", 5));
    await waitFor(() => {
      const lastReq = listThreads.mock.calls.at(-1)?.[0] as ListThreadsRequest;
      expect(lastReq?.labels_any).toEqual(["lookback"]);
    });
    // Single label → label_match is intentionally omitted (server defaults to ANY).
    expect((listThreads.mock.calls.at(-1)?.[0] as ListThreadsRequest).label_match).toBeUndefined();

    // Default mode is AND, so a 2nd selection lands with label_match="all".
    fireEvent.click(barChip("review", 3));
    await waitFor(() => {
      const lastReq = listThreads.mock.calls.at(-1)?.[0] as ListThreadsRequest;
      expect(new Set(lastReq?.labels_any)).toEqual(new Set(["lookback", "review"]));
      expect(lastReq?.label_match).toBe("all");
    });

    // Switching to OR flips the mode on the wire.
    fireEvent.click(screen.getByRole("button", { name: "OR" }));
    await waitFor(() => {
      const lastReq = listThreads.mock.calls.at(-1)?.[0] as ListThreadsRequest;
      expect(lastReq?.label_match).toBe("any");
    });
  });

  it("retains the selected labels when switching to a search mode and forwards them", async () => {
    listThreads.mockResolvedValue([thread("1", [])]);
    findDistinctLabels.mockResolvedValue(labels);
    searchMemoriesKeyword.mockResolvedValue([]);

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={readySidecar} />);

    await openLabelBar();
    await waitFor(() => barChip("lookback", 5));
    fireEvent.click(barChip("lookback", 5));

    fireEvent.click(screen.getByRole("button", { name: "Keyword" }));
    const queryInput = screen.getByPlaceholderText("検索クエリ");
    fireEvent.change(queryInput, { target: { value: "hello" } });

    await waitFor(() => expect(searchMemoriesKeyword).toHaveBeenCalled());
    const req = searchMemoriesKeyword.mock.calls.at(-1)?.[0] as { labels_any?: string[] };
    expect(req?.labels_any).toEqual(["lookback"]);
  });

  it("debounces search input and sends only the latest query", async () => {
    listThreads.mockResolvedValue([]);
    findDistinctLabels.mockResolvedValue(labels);
    searchMemoriesKeyword.mockResolvedValue([]);

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={readySidecar} />);

    fireEvent.click(screen.getByRole("button", { name: "Keyword" }));
    const queryInput = screen.getByPlaceholderText("検索クエリ");
    fireEvent.change(queryInput, { target: { value: "h" } });
    fireEvent.change(queryInput, { target: { value: "he" } });
    fireEvent.change(queryInput, { target: { value: "hello" } });

    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(searchMemoriesKeyword).not.toHaveBeenCalled();

    await waitFor(() => expect(searchMemoriesKeyword).toHaveBeenCalledTimes(1));
    expect(searchMemoriesKeyword.mock.calls[0]?.[0]).toMatchObject({ query_text: "hello" });
  });

  it("toggles a label from the in-card pill without opening the thread modal", async () => {
    listThreads.mockResolvedValue([thread("9", ["lookback", "review"], "Card with labels")]);
    findDistinctLabels.mockResolvedValue(labels);
    findCoOccurringLabels.mockResolvedValue([]);

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={readySidecar} />);

    // Wait for the thread card to render.
    await screen.findByText("Card with labels");

    // The chip in the LabelFilter bar also matches /lookback/, so pick
    // the in-card pill specifically by its role+aria semantics.
    const pills = screen.getAllByRole("button", { name: "lookback" });
    // The card-internal pill is a <span role="button"> (the bar's chip
    // contains `(5)` so its accessible name differs).
    const cardPill = pills.find((p) => p.tagName === "SPAN");
    expect(cardPill).toBeDefined();
    if (!cardPill) return;

    fireEvent.click(cardPill);
    // The modal would render the description in a dialog wrapper — its
    // absence confirms stopPropagation suppressed the card click.
    expect(screen.queryByRole("dialog")).toBeNull();
    // The list call should have been re-issued with the toggled label.
    await waitFor(() => {
      const lastReq = listThreads.mock.calls.at(-1)?.[0] as ListThreadsRequest;
      expect(lastReq?.labels_any).toEqual(["lookback"]);
    });
  });

  it("fetches co-occurring labels in default AND mode and stops when OR is chosen", async () => {
    listThreads.mockResolvedValue([thread("1", [])]);
    findDistinctLabels.mockResolvedValue(labels);
    findCoOccurringLabels.mockResolvedValue([{ label: "review", thread_count: 3 }]);

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={readySidecar} />);

    await openLabelBar();
    await waitFor(() => barChip("lookback", 5));

    // Default is AND — picking one label is enough to trigger co-occurring.
    fireEvent.click(barChip("lookback", 5));
    await waitFor(() => expect(findCoOccurringLabels).toHaveBeenCalled());
    expect(findCoOccurringLabels.mock.calls.at(-1)?.[0]).toEqual({
      user_id: 1,
      labels: ["lookback"],
      limit: 10_000,
    });

    // Pick a second label, then flip to OR; the prior co-occurring call
    // count is the floor — switching to OR must not issue more requests.
    fireEvent.click(barChip("review", 3));
    await waitFor(() => {
      const req = findCoOccurringLabels.mock.calls.at(-1)?.[0] as { labels: string[] };
      expect(new Set(req.labels)).toEqual(new Set(["lookback", "review"]));
    });
    const callsBeforeOr = findCoOccurringLabels.mock.calls.length;
    fireEvent.click(screen.getByRole("button", { name: "OR" }));
    await waitFor(() => {
      const lastReq = listThreads.mock.calls.at(-1)?.[0] as ListThreadsRequest;
      expect(lastReq?.label_match).toBe("any");
    });
    expect(findCoOccurringLabels.mock.calls.length).toBe(callsBeforeOr);
  });

  it("explains short semantic queries without mentioning the embedding runner", async () => {
    listThreads.mockResolvedValue([]);
    findDistinctLabels.mockResolvedValue(labels);
    searchMemoriesSemantic.mockImplementation((req: SearchThreadsRequest) => {
      if (req.query_text.length <= 1) {
        return Promise.reject(new Error("invalid argument: query has too few tokens"));
      }
      return Promise.resolve([]);
    });

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={readySidecar} />);

    fireEvent.click(screen.getByRole("button", { name: "Semantic" }));
    fireEvent.change(screen.getByPlaceholderText("検索クエリ"), { target: { value: "a" } });

    await screen.findByText("invalid argument: query has too few tokens");
    expect(screen.getByText(/検索語をもう少し長く/)).toBeInTheDocument();
    expect(screen.queryByText(/MultimodalEmbeddingRunner/)).toBeNull();
  });

  it("describes empty semantic search without server-side or GPU wording", async () => {
    listThreads.mockResolvedValue([]);
    findDistinctLabels.mockResolvedValue(labels);

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={readySidecar} />);

    fireEvent.click(screen.getByRole("button", { name: "Semantic" }));

    expect(screen.getAllByText("クエリを入力してください").length).toBeGreaterThan(0);
    expect(screen.getByText(/Lookback の embedding worker/)).toBeInTheDocument();
    expect(screen.queryByText(/サーバ側|GPU|MultimodalEmbeddingRunner|未配備/)).toBeNull();
  });

  it("keeps the embedding readiness hint for semantic worker failures", async () => {
    listThreads.mockResolvedValue([]);
    findDistinctLabels.mockResolvedValue(labels);
    searchMemoriesSemantic.mockRejectedValue(new Error("embedding worker unavailable"));

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={readySidecar} />);

    fireEvent.click(screen.getByRole("button", { name: "Semantic" }));
    fireEvent.change(screen.getByPlaceholderText("検索クエリ"), {
      target: { value: "embedding search" },
    });

    await screen.findByText("embedding worker unavailable");
    expect(screen.getByText(/embedding モデルまたは worker/)).toBeInTheDocument();
  });

  it("does not show semantic-specific hints for keyword search errors", async () => {
    listThreads.mockResolvedValue([]);
    findDistinctLabels.mockResolvedValue(labels);
    searchMemoriesKeyword.mockRejectedValue(new Error("keyword search failed"));

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={readySidecar} />);

    fireEvent.click(screen.getByRole("button", { name: "Keyword" }));
    fireEvent.change(screen.getByPlaceholderText("検索クエリ"), { target: { value: "hello" } });

    await screen.findByText("keyword search failed");
    expect(screen.queryByText(/検索語をもう少し長く/)).toBeNull();
    expect(screen.queryByText(/embedding モデルまたは worker/)).toBeNull();
  });
});

describe("Threads page vector-degraded gating", () => {
  const degradedSidecar: SidecarStatus = {
    phase: "ready",
    warnings: [
      {
        kind: "vector_store_degraded",
        message: "degraded",
        detail: JSON.stringify({
          reason: "embedding_dimension_mismatch",
          expected_dim: 2048,
          actual_dim: 768,
        }),
      },
    ],
  };

  beforeEach(() => {
    i18n.changeLanguage("ja");
    listThreads.mockReset();
    findDistinctLabels.mockReset();
    listThreads.mockResolvedValue([]);
    findDistinctLabels.mockResolvedValue([]);
  });

  it("disables semantic and hybrid but keeps keyword/browse when degraded", async () => {
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={degradedSidecar} />);
    await screen.findByRole("button", { name: "一覧" });
    expect(screen.getByRole("button", { name: "Semantic" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Hybrid" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "一覧" })).not.toBeDisabled();
    expect(screen.getByRole("button", { name: "Keyword" })).not.toBeDisabled();
  });

  it("keeps every mode enabled on a healthy sidecar", async () => {
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={readySidecar} />);
    await screen.findByRole("button", { name: "一覧" });
    expect(screen.getByRole("button", { name: "Semantic" })).not.toBeDisabled();
    expect(screen.getByRole("button", { name: "Hybrid" })).not.toBeDisabled();
  });

  it("keeps embedding modes enabled in remote mode even when the local sidecar is degraded", async () => {
    renderWithProviders(
      <Threads onOpenImport={() => {}} sidecar={degradedSidecar} connectionMode="remote" />,
    );
    await screen.findByRole("button", { name: "一覧" });
    expect(screen.getByRole("button", { name: "Semantic" })).not.toBeDisabled();
    expect(screen.getByRole("button", { name: "Hybrid" })).not.toBeDisabled();
  });

  it("does not apply local degraded gating before the connection mode is known", async () => {
    renderWithProviders(
      <Threads onOpenImport={() => {}} sidecar={degradedSidecar} connectionMode={null} />,
    );
    await screen.findByRole("button", { name: "一覧" });
    expect(screen.getByRole("button", { name: "Semantic" })).not.toBeDisabled();
    expect(screen.getByRole("button", { name: "Hybrid" })).not.toBeDisabled();
  });
});
