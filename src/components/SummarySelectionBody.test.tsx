import { render, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import { SummarySelectionBody } from "./SummarySelectionBody";

describe("SummarySelectionBody", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
  });
  it("renders a structured summary with friendly labels, markdown, lists, nested sections, and refs", () => {
    const raw = JSON.stringify({
      title: "リリース準備の要約",
      category: "coding",
      status: "resolved",
      summary: "**目的**を確認しました。",
      key_decisions: ["段階的にリリースする", "ロールバック手順を残す", "監視を追加する"],
      purpose_groups: [
        { purpose: "検証", bullets: ["主要なケースを確認"], source_memory_ids: ["42"] },
      ],
      source_thread_ids: ["20"],
    });

    render(<SummarySelectionBody raw={raw} />);

    expect(screen.getByRole("heading", { name: "リリース準備の要約" })).toBeInTheDocument();
    expect(screen.getByText("カテゴリ: コーディング")).toBeInTheDocument();
    expect(screen.getByText("状態: 解決済み")).toBeInTheDocument();
    expect(screen.getByText("目的")).toBeInTheDocument();
    expect(screen.getByText("を確認しました。", { exact: false })).toBeInTheDocument();
    expect(screen.getByText("段階的にリリースする")).toBeInTheDocument();
    expect(screen.getByText("検証")).toBeInTheDocument();
    expect(screen.getByText("主要なケースを確認")).toBeInTheDocument();
    expect(screen.queryAllByRole("button")).toHaveLength(0);
    expect(screen.queryByText("key_decisions")).not.toBeInTheDocument();
    expect(document.querySelector("pre")).toBeNull();
  });

  it("renders a compact row using title, summary, tags, and at most two decisions", () => {
    const raw = JSON.stringify({
      title: "一覧用タイトル",
      summary: "一覧に表示する概要",
      category: "research",
      status: "ongoing",
      key_decisions: ["決定1", "決定2", "決定3"],
    });

    render(<SummarySelectionBody raw={raw} compact />);

    const row = screen.getByTestId("summary-selection-compact");
    expect(within(row).getByText("一覧用タイトル")).toBeInTheDocument();
    expect(within(row).getByText("一覧に表示する概要")).toBeInTheDocument();
    expect(within(row).getByText("カテゴリ: 調査")).toBeInTheDocument();
    expect(within(row).getByText("状態: 進行中")).toBeInTheDocument();
    expect(within(row).getByText("決定1")).toBeInTheDocument();
    expect(within(row).getByText("決定2")).toBeInTheDocument();
    expect(within(row).queryByText("決定3")).not.toBeInTheDocument();
  });

  it("renders weekly and monthly cards with translated fixed-schema fields", () => {
    render(
      <SummarySelectionBody
        raw={JSON.stringify({
          trends: [
            {
              kind: "continued",
              topic: "認証",
              summary: "継続して改善した",
              source_memory_ids: ["1"],
              owner: "品質チーム",
            },
          ],
          highlights: [{ title: "主要成果", summary: "リリースした" }],
          milestones: [
            {
              title: "v1",
              outcome: "完了",
              completed_in_week: "2026-W32",
              source_memory_ids: ["2"],
            },
          ],
        })}
      />,
    );

    expect(screen.getByText("傾向")).toBeInTheDocument();
    expect(screen.getByText("種別: 継続")).toBeInTheDocument();
    expect(screen.getByText("owner")).toBeInTheDocument();
    expect(screen.getByText("品質チーム")).toBeInTheDocument();
    expect(screen.getByText("ハイライト")).toBeInTheDocument();
    expect(screen.getByText("主要成果")).toBeInTheDocument();
    expect(screen.getByText("マイルストーン")).toBeInTheDocument();
    expect(screen.getByText(/結果:/)).toBeInTheDocument();
    expect(screen.getByText("2026-W32")).toBeInTheDocument();
  });

  it("keeps plain text readable when JSON is malformed", () => {
    render(<SummarySelectionBody raw={"# 旧形式の要約\n\n本文です。"} />);

    expect(screen.getByText("旧形式の要約")).toBeInTheDocument();
    expect(screen.getByText("本文です。")).toBeInTheDocument();
    expect(document.querySelector("pre")).toBeNull();
  });

  it("renders unknown nested fields instead of hiding them", () => {
    render(
      <SummarySelectionBody
        raw={JSON.stringify({
          title: "未知項目",
          custom_block: { label: "値", items: ["a", "b"] },
        })}
      />,
    );

    expect(screen.getByText("custom block")).toBeInTheDocument();
    expect(screen.getByText("label")).toBeInTheDocument();
    expect(screen.getByText("値")).toBeInTheDocument();
    expect(screen.getByText("a")).toBeInTheDocument();
    expect(screen.getByText("b")).toBeInTheDocument();
  });
});
