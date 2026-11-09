import { screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { SUMMARY_GENERATION_STEPS } from "@/hooks/useStagedProgress";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { StagedProgressSnapshot } from "@/types/api";
import { StagedProgressPanel } from "./StagedProgressPanel";

const labels = {
  "thread-summary": "スレッド要約",
  "thread-group-summary": "スレッドグループ要約",
  daily: "日次",
  weekly: "週次",
  monthly: "月次",
};

function snapshot(
  steps: StagedProgressSnapshot<"summary-generation">["steps"],
): StagedProgressSnapshot<"summary-generation"> {
  return { kind: "summary-generation", job_id: "dispatch-1", sequence: 4, steps };
}

beforeEach(() => {
  i18n.changeLanguage("ja");
});

describe("StagedProgressPanel", () => {
  it("shows the overall running state and the active stage's item counter", () => {
    const steps = Object.fromEntries(
      SUMMARY_GENERATION_STEPS.map((step) => [step, { status: "waiting", message: null }]),
    ) as StagedProgressSnapshot<"summary-generation">["steps"];
    steps["thread-summary"] = { status: "active", message: "(2/5) スレッドを要約中" };

    renderWithProviders(
      <StagedProgressPanel
        snapshot={snapshot(steps)}
        title="要約生成"
        labels={labels}
        statusNamespace="analysis"
        showOverall
        onClose={vi.fn()}
        closeLabel="閉じる"
      />,
    );

    expect(screen.getByText("生成中")).toBeInTheDocument();
    expect(screen.getByText("(2/5) スレッドを要約中")).toBeInTheDocument();
    expect(screen.getByText("スレッドグループ要約").parentElement).toHaveTextContent("待機中");
  });

  it("keeps the aggregate result consistent with a warning stage and enables close when settled", () => {
    const steps = Object.fromEntries(
      SUMMARY_GENERATION_STEPS.map((step) => [step, { status: "waiting", message: "スキップ" }]),
    ) as StagedProgressSnapshot<"summary-generation">["steps"];
    steps["thread-summary"] = { status: "done", message: null };
    steps.daily = { status: "warning", message: "成功 4 / 失敗 1" };

    renderWithProviders(
      <StagedProgressPanel
        snapshot={snapshot(steps)}
        title="要約生成"
        labels={labels}
        statusNamespace="analysis"
        showOverall
        onClose={vi.fn()}
        closeLabel="閉じる"
      />,
    );

    expect(screen.getByText("一部失敗")).toBeInTheDocument();
    expect(screen.getByText(/一部失敗 · 成功 4 \/ 失敗 1/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "閉じる" })).toBeEnabled();
  });

  it("shows a partial overall result when failed and successful stages are mixed", () => {
    const steps = Object.fromEntries(
      SUMMARY_GENERATION_STEPS.map((step) => [step, { status: "waiting", message: "スキップ" }]),
    ) as StagedProgressSnapshot<"summary-generation">["steps"];
    steps["thread-summary"] = { status: "done", message: null };
    steps["thread-group-summary"] = { status: "failed", message: "worker failed" };
    steps.daily = { status: "done", message: null };
    steps.weekly = { status: "done", message: null };
    steps.monthly = { status: "done", message: null };

    renderWithProviders(
      <StagedProgressPanel
        snapshot={snapshot(steps)}
        title="要約生成"
        labels={labels}
        statusNamespace="analysis"
        showOverall
        onClose={vi.fn()}
        closeLabel="閉じる"
      />,
    );

    expect(screen.getByText("一部の段で失敗")).toBeInTheDocument();
    expect(screen.getByText("スレッドグループ要約").parentElement).toHaveTextContent(
      "生成失敗: worker failed",
    );
  });

  it("treats a warning stage as partial success when another stage fails", () => {
    const steps = Object.fromEntries(
      SUMMARY_GENERATION_STEPS.map((step) => [step, { status: "waiting", message: "スキップ" }]),
    ) as StagedProgressSnapshot<"summary-generation">["steps"];
    steps["thread-summary"] = { status: "warning", message: "path failed" };
    steps["thread-group-summary"] = { status: "failed", message: "worker failed" };

    renderWithProviders(
      <StagedProgressPanel
        snapshot={snapshot(steps)}
        title="要約生成"
        labels={labels}
        statusNamespace="analysis"
        showOverall
        onClose={vi.fn()}
        closeLabel="閉じる"
      />,
    );

    expect(screen.getByText("一部の段で失敗")).toBeInTheDocument();
    expect(screen.getByText("スレッド要約").parentElement).toHaveTextContent(
      "一部失敗 · path failed",
    );
    expect(screen.getByText("スレッドグループ要約").parentElement).toHaveTextContent(
      "生成失敗: worker failed",
    );
  });

  it("shows completed and skipped stages separately", () => {
    const steps = Object.fromEntries(
      SUMMARY_GENERATION_STEPS.map((step) => [step, { status: "waiting", message: "スキップ" }]),
    ) as StagedProgressSnapshot<"summary-generation">["steps"];
    steps["thread-summary"] = { status: "done", message: null };

    renderWithProviders(
      <StagedProgressPanel
        snapshot={snapshot(steps)}
        title="要約生成"
        labels={labels}
        statusNamespace="analysis"
        showOverall
        onClose={vi.fn()}
        closeLabel="閉じる"
      />,
    );

    expect(screen.getAllByText("生成完了")).toHaveLength(2);
    expect(screen.getByText("スレッド要約").parentElement).toHaveTextContent("完了");
    expect(screen.getByText("日次").parentElement).toHaveTextContent("スキップ");
  });

  it("does not count skipped stages as successes when a stage fails", () => {
    const steps = Object.fromEntries(
      SUMMARY_GENERATION_STEPS.map((step) => [step, { status: "waiting", message: "スキップ" }]),
    ) as StagedProgressSnapshot<"summary-generation">["steps"];
    steps["thread-summary"] = { status: "failed", message: "worker failed" };

    renderWithProviders(
      <StagedProgressPanel
        snapshot={snapshot(steps)}
        title="要約生成"
        labels={labels}
        statusNamespace="analysis"
        showOverall
        onClose={vi.fn()}
        closeLabel="閉じる"
      />,
    );

    expect(screen.getByText("生成失敗")).toBeInTheDocument();
    expect(screen.getByText(/生成失敗: worker failed/)).toBeInTheDocument();
  });

  it("keeps cancellation as the overall result even when an earlier stage completed", () => {
    const steps = Object.fromEntries(
      SUMMARY_GENERATION_STEPS.map((step) => [
        step,
        { status: "cancelled", message: "中断によりスキップ" },
      ]),
    ) as StagedProgressSnapshot<"summary-generation">["steps"];
    steps["thread-summary"] = { status: "done", message: null };
    steps["thread-group-summary"] = { status: "cancelled", message: "中断" };

    renderWithProviders(
      <StagedProgressPanel
        snapshot={snapshot(steps)}
        title="要約生成"
        labels={labels}
        statusNamespace="analysis"
        showOverall
        onClose={vi.fn()}
        closeLabel="閉じる"
      />,
    );

    expect(screen.getAllByText("中断")).toHaveLength(2);
  });

  it("gives cancellation priority over warning when all stages are settled", () => {
    const steps = Object.fromEntries(
      SUMMARY_GENERATION_STEPS.map((step) => [step, { status: "cancelled", message: "中断" }]),
    ) as StagedProgressSnapshot<"summary-generation">["steps"];
    steps["thread-summary"] = { status: "warning", message: "path failed" };

    const { container } = renderWithProviders(
      <StagedProgressPanel
        snapshot={snapshot(steps)}
        title="要約生成"
        labels={labels}
        statusNamespace="analysis"
        showOverall
        onClose={vi.fn()}
        closeLabel="閉じる"
      />,
    );

    expect(container.querySelector(".staged-progress-overall")).toHaveTextContent("中断");
  });

  it("gives cancellation priority over failure while keeping the failed row", () => {
    const steps = Object.fromEntries(
      SUMMARY_GENERATION_STEPS.map((step) => [step, { status: "cancelled", message: "中断" }]),
    ) as StagedProgressSnapshot<"summary-generation">["steps"];
    steps["thread-summary"] = { status: "failed", message: "worker failed" };

    const { container } = renderWithProviders(
      <StagedProgressPanel
        snapshot={snapshot(steps)}
        title="要約生成"
        labels={labels}
        statusNamespace="analysis"
        showOverall
        onClose={vi.fn()}
        closeLabel="閉じる"
      />,
    );

    expect(container.querySelector(".staged-progress-overall")).toHaveTextContent("中断");
    expect(screen.getByText("スレッド要約").parentElement).toHaveTextContent(
      "生成失敗: worker failed",
    );
  });
});
