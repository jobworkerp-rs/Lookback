import { fireEvent, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { defaultSnapshot, IMPORT_STEPS } from "@/hooks/useImportProgress";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { ImportProgressPanel } from "./ImportProgressPanel";

beforeEach(() => {
  void i18n.changeLanguage("ja");
});

function settledSnapshot(jobId: string) {
  const snap = defaultSnapshot(jobId);
  for (const step of IMPORT_STEPS) {
    snap.steps[step] = { status: "done", message: null };
  }
  return snap;
}

it("shows the group summary skip reason instead of waiting", () => {
  const snap = defaultSnapshot("job-skip-group");
  snap.steps["thread-import"] = { status: "done", message: null };
  snap.steps["thread-summary"] = { status: "done", message: null };
  snap.steps["thread-group-summary"] = { status: "waiting", message: "スキップ" };
  renderWithProviders(<ImportProgressPanel snapshot={snap} onClose={vi.fn()} />);
  expect(screen.getByText("thread-group-summary").parentElement?.textContent).toContain("スキップ");
});

it("shows when there are no target threads for group summary", () => {
  const snap = defaultSnapshot("job-empty");
  snap.steps["thread-import"] = { status: "done", message: null };
  snap.steps["thread-group-summary"] = { status: "done", message: "対象スレッドなし" };
  renderWithProviders(<ImportProgressPanel snapshot={snap} onClose={vi.fn()} />);
  expect(screen.getByText("thread-group-summary").parentElement?.textContent).toContain(
    "対象スレッドなし",
  );
});

describe("import progress details", () => {
  it("keeps progress at the top and opens full failure and warning details", () => {
    const snap = settledSnapshot("job-details");
    snap.steps["thread-import"] = {
      status: "failed",
      message: "worker unavailable\nrequest id: 12",
    };
    snap.steps["thread-summary"] = {
      status: "warning",
      message: "成功 9 / 失敗 3 (合計 12)\n直近のエラー: 429 Too Many Requests",
    };

    const { container } = renderWithProviders(
      <ImportProgressPanel snapshot={snap} onClose={vi.fn()} />,
    );

    expect(container.querySelector(".import-progress-panel")).toBeInTheDocument();
    expect(screen.getByText(/一部失敗 · 成功 9 \/ 失敗 3/)).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "詳細" })).toHaveLength(2);

    const detailButtons = screen.getAllByRole("button", { name: "詳細" });
    const failureDetailButton = detailButtons[0];
    const warningDetailButton = detailButtons[1];
    if (!failureDetailButton || !warningDetailButton) {
      throw new Error("Expected failure and warning detail buttons");
    }
    fireEvent.click(failureDetailButton);
    expect(
      screen.getByRole("dialog", { name: "取り込みステップの詳細" }).querySelector("pre")
        ?.textContent,
    ).toBe("worker unavailable\nrequest id: 12");
    fireEvent.click(
      within(screen.getByRole("dialog", { name: "取り込みステップの詳細" })).getByRole("button", {
        name: "閉じる",
      }),
    );

    fireEvent.click(warningDetailButton);
    expect(
      screen.getByRole("dialog", { name: "取り込みステップの詳細" }).querySelector("pre")
        ?.textContent,
    ).toBe("成功 9 / 失敗 3 (合計 12)\n直近のエラー: 429 Too Many Requests");
  });

  it("does not offer details for an empty message", () => {
    const snap = settledSnapshot("job-no-details");
    snap.steps["thread-summary"] = { status: "failed", message: null };
    renderWithProviders(<ImportProgressPanel snapshot={snap} onClose={vi.fn()} />);

    expect(screen.queryByRole("button", { name: "詳細" })).not.toBeInTheDocument();
  });

  it("opens a full multi-line completion message", () => {
    const snap = settledSnapshot("job-completed-details");
    snap.steps.reflection = { status: "done", message: "生成完了\n対象: 4 件" };
    renderWithProviders(<ImportProgressPanel snapshot={snap} onClose={vi.fn()} />);

    fireEvent.click(screen.getByRole("button", { name: "詳細" }));

    expect(
      screen.getByRole("dialog", { name: "取り込みステップの詳細" }).querySelector("pre")
        ?.textContent,
    ).toBe("生成完了\n対象: 4 件");
  });

  it("localizes a no-importable-sources notice in Japanese and English", async () => {
    const snap = settledSnapshot("job-notice-ja");
    snap.steps["thread-import"] = {
      status: "done",
      message: null,
      notice_code: "no-importable-log-sources",
    };
    const view = renderWithProviders(<ImportProgressPanel snapshot={snap} onClose={vi.fn()} />);

    expect(screen.getByText("読み込めるセッションログが見つかりませんでした")).toBeInTheDocument();

    await i18n.changeLanguage("en");
    view.rerender(<ImportProgressPanel snapshot={snap} onClose={vi.fn()} />);

    expect(screen.getByText("No importable session logs were found.")).toBeInTheDocument();
  });
});
