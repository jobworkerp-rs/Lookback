import { fireEvent, render, screen } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, it, vi } from "vitest";
import type { SummaryContent, SummaryValue } from "@/types/api";
import {
  extractSummaryTitle,
  SummaryBody,
  type SummaryRefHandlers,
  SummaryRefHandlersProvider,
} from "./SummaryBody";

function content(parsed: Record<string, SummaryValue> | null, raw: string): SummaryContent {
  return { parsed, raw };
}

function withHandlers(handlers: SummaryRefHandlers, children: ReactNode) {
  return <SummaryRefHandlersProvider value={handlers}>{children}</SummaryRefHandlersProvider>;
}

describe("extractSummaryTitle", () => {
  it("reads the title from a parsed object", () => {
    expect(extractSummaryTitle(content({ title: "T", summary: "S" }, "{}"))).toBe("T");
  });

  it("reads Japanese title aliases from a parsed object", () => {
    expect(extractSummaryTitle(content({ タイトル: "件名です" }, "{}"))).toBe("件名です");
    expect(extractSummaryTitle(content({ 件名: "X" }, "{}"))).toBe("X");
  });

  it("scrapes the title from a truncated JSON snippet", () => {
    // A search snippet cut mid-object: it won't JSON.parse, but the title is
    // near the front and intact.
    const snippet =
      '{"category":"coding","title":"要約タブの構造化表示修正","summary":"## 目的\\n要約タブで構造化…';
    expect(extractSummaryTitle(content(null, snippet))).toBe("要約タブの構造化表示修正");
  });

  it("unescapes escaped quotes inside the scraped title", () => {
    const snippet = '{"title":"foo \\"bar\\" baz","summary":"…';
    expect(extractSummaryTitle(content(null, snippet))).toBe('foo "bar" baz');
  });

  it("prefers a parsed title over the raw body", () => {
    expect(extractSummaryTitle(content({ title: "parsed" }, '{"title":"raw"}'))).toBe("parsed");
  });

  it("returns null when no title is present", () => {
    expect(extractSummaryTitle(content({ summary: "S" }, '{"summary":"S"}'))).toBeNull();
    expect(extractSummaryTitle(content(null, "just some plain text"))).toBeNull();
  });

  it("ignores an empty parsed title and falls back to the raw scrape", () => {
    expect(extractSummaryTitle(content({ title: "  " }, '{"title":"fromraw"}'))).toBe("fromraw");
  });
});

describe("SummaryBody reference chips", () => {
  it("renders source_memory_ids as numbered chips and dispatches onOpenMemoryRef", () => {
    const onOpenMemoryRef = vi.fn();
    render(
      withHandlers(
        { onOpenMemoryRef },
        <SummaryBody parsed={{ source_memory_ids: ["111", "222", "333"] }} raw="{}" />,
      ),
    );
    const chips = screen.getAllByRole("button");
    expect(chips).toHaveLength(3);
    expect(chips.map((c) => c.textContent)).toEqual(["1", "2", "3"]);
    fireEvent.click(chips[1] as HTMLElement);
    expect(onOpenMemoryRef).toHaveBeenCalledWith("222");
  });

  it("renders source_thread_ids chips and routes clicks to onOpenThread", () => {
    const onOpenThread = vi.fn();
    render(
      withHandlers(
        { onOpenThread },
        <SummaryBody parsed={{ source_thread_ids: ["10", "20"] }} raw="{}" />,
      ),
    );
    const chips = screen.getAllByRole("button");
    expect(chips).toHaveLength(2);
    fireEvent.click(chips[0] as HTMLElement);
    expect(onOpenThread).toHaveBeenCalledWith("10");
  });

  it("renders plain string arrays as a bullet list (not chips)", () => {
    render(<SummaryBody parsed={{ bullets: ["one", "two"] }} raw="{}" />);
    expect(screen.queryAllByRole("button")).toHaveLength(0);
    expect(screen.getByText("one")).toBeTruthy();
    expect(screen.getByText("two")).toBeTruthy();
  });

  it("picks up source_memory_ids nested under purpose_groups[]", () => {
    const onOpenMemoryRef = vi.fn();
    render(
      withHandlers(
        { onOpenMemoryRef },
        <SummaryBody
          parsed={{ purpose_groups: [{ purpose: "x", source_memory_ids: ["7"] }] }}
          raw="{}"
        />,
      ),
    );
    const chips = screen.getAllByRole("button");
    expect(chips).toHaveLength(1);
    fireEvent.click(chips[0] as HTMLElement);
    expect(onOpenMemoryRef).toHaveBeenCalledWith("7");
  });

  it("renders static spans (not buttons) when no provider is mounted", () => {
    // Without a SummaryRefHandlersProvider the chip UI still appears (so
    // the reader sees a reference set) but no click target is wired up.
    render(<SummaryBody parsed={{ source_memory_ids: ["1", "2"] }} raw="{}" />);
    expect(screen.queryAllByRole("button")).toHaveLength(0);
    expect(screen.getByText("1")).toBeTruthy();
    expect(screen.getByText("2")).toBeTruthy();
  });
});
