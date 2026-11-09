import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { LabelFilter } from "./LabelFilter";

const labels = [
  // Category (no prefix) — pinned first by LABEL_PREFIX_PRIORITY
  { label: "summary", count: 20 },
  { label: "coding_agent", count: 8 },
  // branch precedes dir in the fixed priority regardless of label count
  { label: "branch:main", count: 7 },
  { label: "dir:/foo", count: 5 },
  { label: "dir:/bar", count: 3 },
];

function noop() {}

describe("LabelFilter", () => {
  it("renders nothing when no labels are available", () => {
    const { container } = render(
      <LabelFilter
        labels={[]}
        selected={[]}
        match="any"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={noop}
      />,
    );
    expect(container).toBeEmptyDOMElement();
  });

  it("orders sections by the fixed prefix priority, not by label count", () => {
    i18n.changeLanguage("ja");
    // agent has the fewest labels but the priority pins it right after the
    // category section; branch precedes dir even though dir has more labels.
    const { container } = renderWithProviders(
      <LabelFilter
        labels={[...labels, { label: "agent:codex", count: 1 }]}
        selected={["summary"]} // open the fold
        match="any"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={noop}
      />,
    );
    const prefixes = [...container.querySelectorAll(".label-filter-section-prefix")].map(
      (el) => el.textContent,
    );
    // Category heading is translated; the rest are raw prefixes in priority order.
    expect(prefixes).toEqual(["カテゴリ", "agent", "branch", "dir"]);
  });

  it("starts collapsed when nothing is selected", () => {
    const { container } = render(
      <LabelFilter
        labels={labels}
        selected={[]}
        match="any"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={noop}
      />,
    );
    const details = container.querySelector("details");
    expect(details).not.toBeNull();
    expect(details?.hasAttribute("open")).toBe(false);
  });

  it("auto-expands when a selection exists", () => {
    const { container } = render(
      <LabelFilter
        labels={labels}
        selected={["summary"]}
        match="any"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={noop}
      />,
    );
    expect(container.querySelector("details")?.hasAttribute("open")).toBe(true);
  });

  it("omits the count when a chip has no count", () => {
    render(
      <LabelFilter
        labels={[{ label: "source:repo" }]}
        selected={["source:repo"]}
        match="any"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={noop}
      />,
    );
    expect(screen.getByRole("button", { name: "source:repo" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /source:repo \(/ })).toBeNull();
  });

  it("shows selected labels in the summary title and clears them without toggling the fold", () => {
    const onClear = vi.fn();
    const { container, rerender } = render(
      <LabelFilter
        labels={labels}
        selected={["summary", "branch:main"]}
        match="any"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={noop}
        onClear={onClear}
      />,
    );
    const summary = screen.getByText("ラベルで絞り込む (2)");
    expect(summary.closest("summary")).toHaveAttribute("title", "summary, branch:main");
    const details = summary.closest("details");
    expect(details).toHaveAttribute("open");
    fireEvent.click(screen.getByRole("button", { name: "クリア" }));
    expect(onClear).toHaveBeenCalledOnce();
    expect(details).toHaveAttribute("open");
    rerender(
      <LabelFilter
        labels={labels}
        selected={[]}
        match="any"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={noop}
        onClear={onClear}
      />,
    );
    expect(container.querySelector("details")).not.toHaveAttribute("open");
  });

  it("marks a selected chip as pressed", () => {
    render(
      <LabelFilter
        labels={labels}
        selected={["summary"]}
        match="any"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={noop}
      />,
    );
    const chip = screen.getByRole("button", { name: /summary \(20\)/ });
    expect(chip).toHaveAttribute("aria-pressed", "true");
  });

  it("calls onToggle when a chip is clicked", () => {
    const onToggle = vi.fn();
    render(
      <LabelFilter
        labels={labels}
        selected={["summary"]}
        match="any"
        onToggle={onToggle}
        onToggleMany={noop}
        onSetMatch={noop}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /branch:main \(7\)/ }));
    expect(onToggle).toHaveBeenCalledWith("branch:main");
  });

  it("hides the AND/OR toggle while fewer than 2 labels are selected", () => {
    render(
      <LabelFilter
        labels={labels}
        selected={["summary"]}
        match="any"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={noop}
      />,
    );
    expect(screen.queryByRole("button", { name: "AND" })).toBeNull();
  });

  it("shows the AND/OR toggle once 2+ labels are selected and reports the change", () => {
    const onSetMatch = vi.fn();
    render(
      <LabelFilter
        labels={labels}
        selected={["summary", "branch:main"]}
        match="any"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={onSetMatch}
      />,
    );
    expect(screen.getByRole("button", { name: "OR" })).toHaveAttribute("aria-pressed", "true");
    fireEvent.click(screen.getByRole("button", { name: "AND" }));
    expect(onSetMatch).toHaveBeenCalledWith("all");
  });

  it("can hide the AND/OR toggle for a fixed-match consumer", () => {
    render(
      <LabelFilter
        labels={labels}
        selected={["summary", "branch:main"]}
        match="all"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={noop}
        showMatchToggle={false}
      />,
    );
    expect(screen.queryByRole("button", { name: "AND" })).toBeNull();
    expect(screen.queryByRole("button", { name: "OR" })).toBeNull();
  });

  // Find the section heading button by its visible prefix text (the
  // button renders <prefix><count><action> as inline spans).
  const sectionHead = (prefix: string) =>
    screen
      .getAllByRole("button")
      .find(
        (b) =>
          b.classList.contains("label-filter-section-head") && b.textContent?.startsWith(prefix),
      );

  it("section heading turns on every label in the prefix when none are selected", () => {
    const onToggleMany = vi.fn();
    render(
      <LabelFilter
        labels={labels}
        selected={["summary"]}
        match="any"
        onToggle={noop}
        onToggleMany={onToggleMany}
        onSetMatch={noop}
      />,
    );
    const head = sectionHead("dir");
    expect(head).toBeDefined();
    if (!head) return;
    fireEvent.click(head);
    expect(onToggleMany).toHaveBeenCalled();
    const [labelsArg, turnOn] = onToggleMany.mock.calls[0] as [string[], boolean];
    expect(new Set(labelsArg)).toEqual(new Set(["dir:/foo", "dir:/bar"]));
    expect(turnOn).toBe(true);
  });

  it("section heading turns off every label when all in the prefix are selected", () => {
    const onToggleMany = vi.fn();
    render(
      <LabelFilter
        labels={labels}
        selected={["dir:/foo", "dir:/bar"]}
        match="any"
        onToggle={noop}
        onToggleMany={onToggleMany}
        onSetMatch={noop}
      />,
    );
    const head = sectionHead("dir");
    expect(head).toBeDefined();
    if (!head) return;
    fireEvent.click(head);
    const [, turnOn] = onToggleMany.mock.calls[0] as [string[], boolean];
    expect(turnOn).toBe(false);
  });

  it("hides labels not in coOccurringLabels (selection still shown)", () => {
    render(
      <LabelFilter
        labels={labels}
        coOccurringLabels={[{ label: "branch:main", count: 7 }]}
        selected={["summary"]}
        match="all"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={noop}
      />,
    );
    // Selected label remains.
    expect(screen.getByRole("button", { name: /summary \(20\)/ })).toBeInTheDocument();
    // Co-occurring label remains.
    expect(screen.getByRole("button", { name: /branch:main \(7\)/ })).toBeInTheDocument();
    // Non-matching labels are hidden.
    expect(screen.queryByRole("button", { name: /dir:\/foo/ })).toBeNull();
    expect(screen.queryByRole("button", { name: /coding_agent/ })).toBeNull();
  });

  it("renders the co-occurrence count (not the global distinct count) for narrowed chips", () => {
    // distinct says branch:main has 7 threads globally; co-occurring says
    // only 2 of those carry the current selection. The chip must show the
    // intersection count — that's the size of the next click's narrowing.
    render(
      <LabelFilter
        labels={labels}
        coOccurringLabels={[{ label: "branch:main", count: 2 }]}
        selected={["summary"]}
        match="all"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={noop}
      />,
    );
    expect(screen.getByRole("button", { name: /branch:main \(2\)/ })).toBeInTheDocument();
    // Selected anchor keeps the global distinct count (proto excludes it
    // from the co-occurring response).
    expect(screen.getByRole("button", { name: /summary \(20\)/ })).toBeInTheDocument();
  });

  it("shows the full distinct list when coOccurringLabels is omitted", () => {
    render(
      <LabelFilter
        labels={labels}
        selected={["summary"]}
        match="all"
        onToggle={noop}
        onToggleMany={noop}
        onSetMatch={noop}
      />,
    );
    expect(screen.getByRole("button", { name: /dir:\/foo/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /coding_agent/ })).toBeInTheDocument();
  });
});
