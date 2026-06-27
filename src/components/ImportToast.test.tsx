import { fireEvent, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { defaultSnapshot, IMPORT_STEPS, type ImportSnapshot } from "@/hooks/useImportProgress";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { ImportToast } from "./ImportToast";

function busySnapshot(): ImportSnapshot {
  // defaultSnapshot starts with `thread-import` active — that alone
  // satisfies the "any step is active" rule for `busy`.
  return defaultSnapshot("job-import-1");
}

function terminalSnapshot(): ImportSnapshot {
  const snap = defaultSnapshot("job-import-1");
  // Every step settled (done) → the toast becomes a results panel and
  // the cancel row must disappear.
  for (const step of IMPORT_STEPS) {
    snap.steps[step] = { status: "done", message: null };
  }
  return snap;
}

beforeEach(() => {
  i18n.changeLanguage("ja");
});

describe("ImportToast cancel row", () => {
  it("shows 中断 while any step is still active", () => {
    renderWithProviders(
      <ImportToast snapshot={busySnapshot()} onClose={vi.fn()} onCancel={vi.fn()} />,
    );
    // The button is positioned in the footer of the toast (not in the
    // header where the ✕ Dismiss lives), so the label-based match is
    // enough — distinct from the header close button.
    expect(screen.getByRole("button", { name: "中断" })).toBeTruthy();
  });

  it("invokes onCancel when the user clicks 中断", () => {
    const onCancel = vi.fn();
    renderWithProviders(
      <ImportToast snapshot={busySnapshot()} onClose={vi.fn()} onCancel={onCancel} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "中断" }));
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("hides 中断 once every step is terminal (results panel mode)", () => {
    renderWithProviders(
      <ImportToast snapshot={terminalSnapshot()} onClose={vi.fn()} onCancel={vi.fn()} />,
    );
    expect(screen.queryByRole("button", { name: "中断" })).toBeNull();
    // The ✕ Dismiss button stays so the user can still close the toast.
    expect(screen.getByRole("button", { name: "閉じる" })).toBeTruthy();
  });
});

describe("ImportToast warning status", () => {
  it("renders 一部失敗 + counter digest + 詳細 affordance for a partial-failure step", () => {
    // A summary batch that finished with `failed_count > 0` — Rust side
    // emits StepStatus::Warning with the "成功 N / 失敗 M" digest. The
    // toast must surface this distinct from a green 完了 so the user
    // knows the 429 storm degraded the run.
    const snap = defaultSnapshot("job-import-2");
    for (const step of IMPORT_STEPS) {
      snap.steps[step] = { status: "done", message: null };
    }
    snap.steps["thread-summary"] = {
      status: "warning",
      message: "成功 9 / 失敗 3 (合計 12)\n直近のエラー: 429 Too Many Requests",
    };
    renderWithProviders(<ImportToast snapshot={snap} onClose={vi.fn()} onCancel={vi.fn()} />);

    // The status label is "一部失敗 · 成功 9 / 失敗 3 (...)" — the counter
    // line rides next to the badge so the partial result is visible at
    // a glance.
    const label = screen.getByText(/一部失敗/);
    expect(label.textContent).toContain("成功 9");
    expect(label.textContent).toContain("失敗 3");

    // 詳細 button must be present (warning is multi-line) so the user can
    // open the full error context — same affordance as a hard failure.
    expect(screen.getByRole("button", { name: "詳細" })).toBeTruthy();
  });
});
