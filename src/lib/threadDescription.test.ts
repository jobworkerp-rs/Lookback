import { describe, expect, it } from "vitest";
import { parseThreadDescription } from "./threadDescription";

describe("parseThreadDescription", () => {
  it("splits the YAML-produced 【title】 body shape", () => {
    // Matches `"【" + title + "】 " + summary` from thread-summary-single.yaml.
    const r = parseThreadDescription("【設計レビュー】 ## 目的\n要約本文", "fallback");
    expect(r.title).toBe("設計レビュー");
    expect(r.body).toBe("## 目的\n要約本文");
  });

  it("treats a title without a trailing body as title-only", () => {
    const r = parseThreadDescription("【タイトルのみ】", "fallback");
    expect(r.title).toBe("タイトルのみ");
    expect(r.body).toBeNull();
  });

  it("tolerates a missing space after the close bracket", () => {
    const r = parseThreadDescription("【T】本文", "fallback");
    expect(r.title).toBe("T");
    expect(r.body).toBe("本文");
  });

  it("returns an un-summarized plain description as the title", () => {
    const r = parseThreadDescription("元のスレッドタイトル", "fallback");
    expect(r.title).toBe("元のスレッドタイトル");
    expect(r.body).toBeNull();
  });

  it("uses the fallback for null / empty / whitespace", () => {
    expect(parseThreadDescription(null, "Thread #1")).toEqual({ title: "Thread #1", body: null });
    expect(parseThreadDescription(undefined, "Thread #1")).toEqual({
      title: "Thread #1",
      body: null,
    });
    expect(parseThreadDescription("   ", "Thread #1")).toEqual({ title: "Thread #1", body: null });
  });

  it("falls back when the bracket pair is empty", () => {
    const r = parseThreadDescription("【】 body", "fallback");
    expect(r.title).toBe("fallback");
    expect(r.body).toBe("body");
  });

  it("does not treat a non-leading bracket as a title delimiter", () => {
    const r = parseThreadDescription("prefix【inner】rest", "fallback");
    expect(r.title).toBe("prefix【inner】rest");
    expect(r.body).toBeNull();
  });
});
