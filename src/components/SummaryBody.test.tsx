import { fireEvent, render, screen } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
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

describe("SummaryBody period collections", () => {
  const periodSummary = {
    overall_purpose: "概要は常に読めます",
    purpose_groups: [
      { purpose: "リリース準備", status: "ongoing", bullets: ["検証を完了"] },
      { purpose: "品質改善", bullets: ["回帰を確認"] },
    ],
    by_topic: [{ topic: "API", bullets: ["契約を更新"] }],
  } as Record<string, SummaryValue>;

  it("starts structured period items collapsed while retaining the overview", () => {
    i18n.changeLanguage("ja");
    const { container } = render(
      <SummaryBody parsed={periodSummary} raw="{}" periodDisplay={{ initialMode: "collapsed" }} />,
    );

    expect(screen.getByText("概要は常に読めます")).toBeTruthy();
    expect(screen.getByText("リリース準備")).toBeTruthy();
    expect(container.querySelectorAll(".summary-structured-card[open]")).toHaveLength(0);
    expect(screen.getByRole("button", { name: "すべて展開" })).toBeTruthy();
  });

  it("expands and collapses all structured items inside one period summary", () => {
    i18n.changeLanguage("ja");
    const { container } = render(
      <SummaryBody parsed={periodSummary} raw="{}" periodDisplay={{ initialMode: "collapsed" }} />,
    );

    fireEvent.click(screen.getByRole("button", { name: "すべて展開" }));
    expect(screen.getByText("検証を完了")).toBeTruthy();
    expect(screen.getByText("契約を更新")).toBeTruthy();
    expect(container.querySelectorAll(".summary-structured-card[open]")).toHaveLength(3);

    fireEvent.click(screen.getByRole("button", { name: "すべて折りたたむ" }));
    expect(container.querySelectorAll(".summary-structured-card[open]")).toHaveLength(0);
  });

  it("keeps generic rendering for non-period summaries", () => {
    render(<SummaryBody parsed={periodSummary} raw="{}" />);
    expect(screen.getByText("検証を完了")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "すべて展開" })).toBeNull();
  });

  it("keeps every value visible when a structured collection contains a scalar", () => {
    render(
      <SummaryBody
        parsed={{
          purpose_groups: [{ purpose: "確認", bullets: ["詳細"] }, "補足の本文"],
        }}
        raw="{}"
        periodDisplay={{ initialMode: "collapsed" }}
      />,
    );

    expect(screen.getByText("詳細")).toBeTruthy();
    expect(screen.getByText("補足の本文")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "すべて展開" })).toBeNull();
  });

  it("renders structured item counts in English", () => {
    i18n.changeLanguage("en");
    render(
      <SummaryBody
        parsed={{ purpose_groups: [{ purpose: "Purpose", bullets: ["one", "two"] }] }}
        raw="{}"
        periodDisplay={{ initialMode: "collapsed" }}
      />,
    );

    expect(screen.getByText("2 items")).toBeTruthy();
    i18n.changeLanguage("ja");
  });
});
