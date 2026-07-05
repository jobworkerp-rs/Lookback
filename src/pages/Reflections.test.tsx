import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ReflectionProgressHandle } from "@/hooks/useReflectionProgress";
import type { SidecarStatus } from "@/hooks/useSidecarStatus";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { ReflectionEntry } from "@/types/api";
import { ReflectionCard, Reflections } from "./Reflections";

const searchReflections = vi.fn();
const searchReflectionsByIntent = vi.fn();
const deleteReflection = vi.fn();
const enqueueReflectionJob = vi.fn();

vi.mock("@/api", () => ({
  searchReflections: (req: unknown) => searchReflections(req),
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

const readySidecar: SidecarStatus = { phase: "ready", warnings: [] };
const degradedSidecar: SidecarStatus = {
  phase: "ready",
  warnings: [{ kind: "vector_store_degraded", message: "degraded", detail: null }],
};

const reflectionProgress: ReflectionProgressHandle = {
  progress: null,
  start: vi.fn(),
  cancel: vi.fn(),
  clear: vi.fn(),
};

function renderReflections(sidecar: SidecarStatus) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const ui = (nextSidecar: SidecarStatus) => (
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <Reflections reflectionProgress={reflectionProgress} sidecar={nextSidecar} />
      </QueryClientProvider>
    </I18nextProvider>
  );
  const result = render(ui(sidecar));
  return {
    ...result,
    rerenderWithSidecar: (nextSidecar: SidecarStatus) => result.rerender(ui(nextSidecar)),
  };
}

beforeEach(() => {
  i18n.changeLanguage("ja");
  searchReflections.mockReset();
  searchReflectionsByIntent.mockReset();
  deleteReflection.mockReset();
  enqueueReflectionJob.mockReset();
  searchReflections.mockResolvedValue([]);
  searchReflectionsByIntent.mockResolvedValue([]);
});

describe("Reflections vector-degraded gating", () => {
  it("falls back to structured search instead of reusing stale intent text when degraded", async () => {
    const view = renderReflections(readySidecar);
    await waitFor(() => expect(searchReflections).toHaveBeenCalled());

    fireEvent.change(screen.getByPlaceholderText(/意図テキスト/), {
      target: { value: "フレーキーなテスト" },
    });
    await waitFor(() => expect(searchReflectionsByIntent).toHaveBeenCalledTimes(1));

    view.rerenderWithSidecar(degradedSidecar);

    await waitFor(() => expect(screen.getByPlaceholderText(/ベクトルストア/)).toBeDisabled());
    await waitFor(() => expect(searchReflections).toHaveBeenCalledTimes(2));
    expect(searchReflectionsByIntent).toHaveBeenCalledTimes(1);
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
