import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { renderWithProviders } from "@/test-utils";
import type { ReflectionEntry } from "@/types/api";
import { ReflectionCard } from "./Reflections";

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
