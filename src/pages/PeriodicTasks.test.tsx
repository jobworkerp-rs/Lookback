import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  cancelPeriodicExecution,
  createPeriodicTask,
  deletePeriodicTask,
  listPeriodicExecutionHistory,
  listPeriodicTaskStatuses,
  listPeriodicTasks,
  setEnabledPeriodicTask,
} from "@/api";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type {
  PeriodicExecutionHistoryEntry,
  PeriodicExecutionStatus,
  PeriodicExecutionSummary,
  PeriodicTaskEntry,
} from "@/types/api";
import { PeriodicTasks } from "./PeriodicTasks";

vi.mock("@/api", () => ({
  cancelPeriodicExecution: vi.fn(),
  createPeriodicTask: vi.fn(),
  deletePeriodicTask: vi.fn(),
  listPeriodicExecutionHistory: vi.fn(),
  listPeriodicTasks: vi.fn(),
  listPeriodicTaskStatuses: vi.fn(),
  setEnabledPeriodicTask: vi.fn(),
  updatePeriodicTask: vi.fn(),
}));

const mockListPeriodicTasks = vi.mocked(listPeriodicTasks);
const mockCreatePeriodicTask = vi.mocked(createPeriodicTask);
const mockDeletePeriodicTask = vi.mocked(deletePeriodicTask);
const mockSetEnabledPeriodicTask = vi.mocked(setEnabledPeriodicTask);
const mockListStatuses = vi.mocked(listPeriodicTaskStatuses);
const mockListHistory = vi.mocked(listPeriodicExecutionHistory);
const mockCancel = vi.mocked(cancelPeriodicExecution);

function summary(
  status: PeriodicExecutionStatus,
  overrides: Partial<PeriodicExecutionSummary> = {},
): PeriodicExecutionSummary {
  const active = ["pending", "running", "wait_result", "cancelling"].includes(status);
  const cancelable = ["pending", "running", "wait_result"].includes(status);
  const hasRuntime = status !== "not_started" && status !== "unavailable";
  return {
    scheduler_id: "42",
    status,
    active,
    cancelable,
    error: null,
    runtime: hasRuntime
      ? {
          execution_ref_id: "777",
          scheduler_id: "42",
          scheduler_name: "朝の要約",
          job_id: "12345",
          status,
          status_source: "job_processing_status",
          triggered_at_ms: 1_700_000_000_000,
          observed_at_ms: 1_700_000_005_000,
          detail: null,
          enqueue_error: null,
          active,
          cancelable,
        }
      : null,
    ...overrides,
  };
}

function historyEntry(
  status: PeriodicExecutionStatus,
  overrides: Partial<PeriodicExecutionHistoryEntry> = {},
): PeriodicExecutionHistoryEntry {
  const active = ["pending", "running", "wait_result", "cancelling"].includes(status);
  const cancelable = ["pending", "running", "wait_result"].includes(status);
  return {
    execution_ref_id: "777",
    scheduler_id: "42",
    scheduler_name: "朝の要約",
    job_id: "12345",
    status,
    status_source: "job_processing_status",
    triggered_at_ms: 1_700_000_000_000,
    observed_at_ms: 1_700_000_005_000,
    detail: null,
    enqueue_error: null,
    active,
    cancelable,
    trigger_context_json: '{"k":1}',
    created_at_ms: 1_700_000_001_000,
    ...overrides,
  };
}

function task(overrides: Partial<PeriodicTaskEntry> = {}): PeriodicTaskEntry {
  return {
    id: "42",
    name: "朝の要約",
    enabled: true,
    crontab: "0 0 9 * * *",
    description: null,
    status: "supported",
    task: {
      name: "朝の要約",
      source: "codex",
      sources: ["codex"],
      task_kind: "regular",
      hour: 9,
      minute: 0,
      interval_hours: 24,
      interval_days: null,
      weekly_day: null,
      monthly_day: null,
      lookback_days: 7,
      force_thread_summary: true,
      run_summary_daily: true,
      run_personality: true,
      run_reflection: true,
    },
    ...overrides,
  };
}

describe("PeriodicTasks", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    mockListPeriodicTasks.mockReset();
    mockCreatePeriodicTask.mockReset();
    mockDeletePeriodicTask.mockReset();
    mockSetEnabledPeriodicTask.mockReset();
    mockListStatuses.mockReset();
    mockListHistory.mockReset();
    mockCancel.mockReset();
    mockListPeriodicTasks.mockResolvedValue([]);
    mockCreatePeriodicTask.mockResolvedValue("new-id");
    mockDeletePeriodicTask.mockResolvedValue(undefined);
    mockSetEnabledPeriodicTask.mockResolvedValue(undefined);
    mockListStatuses.mockResolvedValue([]);
    mockListHistory.mockResolvedValue([]);
    mockCancel.mockResolvedValue(undefined);
  });

  it("renders supported and unsupported schedulers without hiding legacy rows", async () => {
    mockListPeriodicTasks.mockResolvedValue([
      task(),
      task({
        id: "99",
        name: "旧形式",
        crontab: "0 30 9 * * 1",
        status: "unsupported",
        task: null,
      }),
    ]);

    renderWithProviders(<PeriodicTasks />);

    expect(await screen.findByText("朝の要約")).toBeInTheDocument();
    expect(screen.getByText("定期")).toBeInTheDocument();
    expect(screen.getByText("0 0 9 * * *")).toBeInTheDocument();
    expect(screen.getByText("旧形式")).toBeInTheDocument();
    expect(screen.getByText("unsupported")).toBeInTheDocument();
    expect(screen.getByText("未対応形式")).toBeInTheDocument();
    expect(screen.getByText("0 30 9 * * 1")).toBeInTheDocument();
  });

  it("creates a local periodic task from the dialog", async () => {
    renderWithProviders(<PeriodicTasks />);

    fireEvent.click(await screen.findByRole("button", { name: "新規" }));
    fireEvent.change(screen.getByLabelText("名前"), { target: { value: "昼の要約" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => {
      expect(mockCreatePeriodicTask).toHaveBeenCalledWith({
        task: expect.objectContaining({
          name: "昼の要約",
          source: "codex",
          task_kind: "regular",
          interval_hours: 24,
          lookback_days: 7,
          // A name-only save must not be a no-op: the basic summary stages
          // are enabled by default.
          force_thread_summary: true,
          run_summary_daily: true,
        }),
        enabled: true,
        description: null,
      });
    });
  });

  it("lets regular tasks opt out of every generated output for an import-only run", async () => {
    renderWithProviders(<PeriodicTasks />);

    fireEvent.click(await screen.findByRole("button", { name: "新規" }));
    expect(screen.getByText("生成内容")).toBeInTheDocument();
    // Thread + daily summary default on; personality / reflection default off.
    expect(screen.getByLabelText("要約: 日次")).toBeChecked();
    expect(screen.getByLabelText("要約: スレッド")).toBeChecked();
    expect(screen.getByLabelText("パーソナリティ")).not.toBeChecked();
    expect(screen.getByLabelText("自省")).not.toBeChecked();

    // Explicitly opt out: daily first (it disables/forces thread), then thread.
    fireEvent.click(screen.getByLabelText("要約: 日次"));
    fireEvent.click(screen.getByLabelText("要約: スレッド"));
    expect(screen.getByLabelText("要約: 日次")).not.toBeChecked();
    expect(screen.getByLabelText("要約: スレッド")).not.toBeChecked();

    fireEvent.change(screen.getByLabelText("名前"), { target: { value: "取り込みのみ" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => {
      expect(mockCreatePeriodicTask).toHaveBeenCalledWith({
        task: expect.objectContaining({
          name: "取り込みのみ",
          force_thread_summary: false,
          run_summary_daily: false,
          run_personality: false,
          run_reflection: false,
        }),
        enabled: true,
        description: null,
      });
    });
  });

  it("keeps thread summary selected while daily summary is selected", async () => {
    renderWithProviders(<PeriodicTasks />);

    fireEvent.click(await screen.findByRole("button", { name: "新規" }));
    // Daily defaults on, which forces and disables the thread checkbox.
    expect(screen.getByLabelText("要約: スレッド")).toBeChecked();
    expect(screen.getByLabelText("要約: スレッド")).toBeDisabled();

    // Toggling daily off re-enables thread; turning it back on re-forces thread.
    fireEvent.click(screen.getByLabelText("要約: 日次"));
    expect(screen.getByLabelText("要約: スレッド")).not.toBeDisabled();
    fireEvent.click(screen.getByLabelText("要約: 日次"));
    expect(screen.getByLabelText("要約: スレッド")).toBeChecked();
    expect(screen.getByLabelText("要約: スレッド")).toBeDisabled();

    fireEvent.change(screen.getByLabelText("名前"), { target: { value: "日次あり" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => {
      expect(mockCreatePeriodicTask).toHaveBeenCalledWith({
        task: expect.objectContaining({
          name: "日次あり",
          force_thread_summary: true,
          run_summary_daily: true,
        }),
        enabled: true,
        description: null,
      });
    });
  });

  it("offers supported regular hour intervals only", async () => {
    renderWithProviders(<PeriodicTasks />);

    fireEvent.click(await screen.findByRole("button", { name: "新規" }));

    const intervalSelect = screen.getByLabelText("間隔");
    expect(intervalSelect).toHaveRole("combobox");
    expect(within(intervalSelect).getByRole("option", { name: "8時間" })).toBeInTheDocument();
    expect(within(intervalSelect).queryByRole("option", { name: "5時間" })).not.toBeInTheDocument();

    fireEvent.change(screen.getByLabelText("名前"), { target: { value: "8時間おき" } });
    fireEvent.change(intervalSelect, { target: { value: "8" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => {
      expect(mockCreatePeriodicTask).toHaveBeenCalledWith({
        task: expect.objectContaining({
          name: "8時間おき",
          interval_hours: 8,
          interval_days: null,
        }),
        enabled: true,
        description: null,
      });
    });
  });

  it("offers supported source modes only and creates a combined-source task", async () => {
    renderWithProviders(<PeriodicTasks />);

    fireEvent.click(await screen.findByRole("button", { name: "新規" }));
    const sourceSelect = screen.getByLabelText("対象ソース");
    expect(within(sourceSelect).queryByRole("option", { name: "plain" })).not.toBeInTheDocument();

    fireEvent.change(screen.getByLabelText("名前"), { target: { value: "まとめて要約" } });
    fireEvent.change(sourceSelect, { target: { value: "codex+claude-code" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => {
      expect(mockCreatePeriodicTask).toHaveBeenCalledWith({
        task: expect.objectContaining({
          name: "まとめて要約",
          source: "codex+claude-code",
          sources: ["codex", "claude-code"],
        }),
        enabled: true,
        description: null,
      });
    });
  });

  it("keeps local conductor controls enabled when remote connection mode is configured elsewhere", async () => {
    mockListPeriodicTasks.mockResolvedValue([task()]);

    renderWithProviders(<PeriodicTasks />);

    expect(await screen.findByText("朝の要約")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "新規" })).not.toBeDisabled();
    expect(screen.getByRole("button", { name: "編集" })).not.toBeDisabled();
    expect(screen.getByLabelText("有効")).not.toBeDisabled();
  });

  it("keeps delete available and calls the delete command after confirmation", async () => {
    mockListPeriodicTasks.mockResolvedValue([task()]);
    renderWithProviders(<PeriodicTasks />);

    fireEvent.click(await screen.findByRole("button", { name: "削除" }));
    fireEvent.click(
      within(screen.getByRole("dialog", { name: "定期実行を削除します" })).getByRole("button", {
        name: "削除",
      }),
    );

    await waitFor(() => expect(mockDeletePeriodicTask).toHaveBeenCalledWith("42"));
  });

  it("toggles enabled state for supported local schedulers", async () => {
    mockListPeriodicTasks.mockResolvedValue([task()]);
    renderWithProviders(<PeriodicTasks />);

    fireEvent.click(await screen.findByLabelText("有効"));

    await waitFor(() => expect(mockSetEnabledPeriodicTask).toHaveBeenCalledWith("42", false));
  });

  it("shows seeded disabled schedules and lets the user enable them", async () => {
    mockListPeriodicTasks.mockResolvedValue([
      task({
        id: "seed-daily",
        name: "Daily import and summaries",
        enabled: false,
        crontab: "0 0 0 */1 * *",
        task: {
          name: "Daily import and summaries",
          source: "codex+claude-code",
          sources: ["codex", "claude-code"],
          task_kind: "regular",
          hour: 0,
          minute: 0,
          interval_hours: null,
          interval_days: 1,
          weekly_day: null,
          monthly_day: null,
          lookback_days: 1,
          force_thread_summary: true,
          run_summary_daily: true,
          run_personality: false,
          run_reflection: false,
        },
      }),
    ]);
    renderWithProviders(<PeriodicTasks />);

    expect(await screen.findByText("Daily import and summaries")).toBeInTheDocument();
    expect(screen.getByText("無効")).toBeInTheDocument();
    expect(screen.getByText("codex + claude-code")).toBeInTheDocument();

    fireEvent.click(screen.getByLabelText("有効"));

    await waitFor(() =>
      expect(mockSetEnabledPeriodicTask).toHaveBeenCalledWith("seed-daily", true),
    );
  });

  it("shows 未実行 when the scheduler has never run", async () => {
    mockListPeriodicTasks.mockResolvedValue([task()]);
    mockListStatuses.mockResolvedValue([summary("not_started")]);
    renderWithProviders(<PeriodicTasks />);

    expect(await screen.findByText("未実行")).toBeInTheDocument();
    expect(screen.getByText("まだ実行されていません")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "停止" })).not.toBeInTheDocument();
  });

  it("shows 実行中 and a 停止 button when the latest execution is running", async () => {
    mockListPeriodicTasks.mockResolvedValue([task()]);
    mockListStatuses.mockResolvedValue([summary("running")]);
    renderWithProviders(<PeriodicTasks />);

    expect(await screen.findByText("実行中")).toBeInTheDocument();
    expect(screen.getByText(/job 12345/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "停止" })).toBeInTheDocument();
  });

  it("shows 取消中 without a 停止 button while cancelling", async () => {
    mockListPeriodicTasks.mockResolvedValue([task()]);
    mockListStatuses.mockResolvedValue([summary("cancelling")]);
    renderWithProviders(<PeriodicTasks />);

    expect(await screen.findByText("取消中")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "停止" })).not.toBeInTheDocument();
  });

  it("shows 成功 and the latest time when the execution succeeded", async () => {
    mockListPeriodicTasks.mockResolvedValue([task()]);
    mockListStatuses.mockResolvedValue([summary("succeeded")]);
    renderWithProviders(<PeriodicTasks />);

    expect(await screen.findByText("成功")).toBeInTheDocument();
    expect(screen.getByText(/最新:/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "停止" })).not.toBeInTheDocument();
  });

  it("shows 状態を取得できません (not a permanent 取得中…) when the status query rejects", async () => {
    mockListPeriodicTasks.mockResolvedValue([task()]);
    mockListStatuses.mockRejectedValue(new Error("conductor down"));
    renderWithProviders(<PeriodicTasks />);

    // The card itself stays visible even though status resolution failed.
    expect(await screen.findByText("朝の要約")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "履歴" })).toBeInTheDocument();
    // A whole-query rejection must surface as an explicit failure, not a
    // perpetual loading state the user can't distinguish from an outage.
    expect(await screen.findByText(/状態を取得できません/)).toBeInTheDocument();
    expect(screen.queryByText("最新: 取得中…")).not.toBeInTheDocument();
  });

  it("opens the history modal and shows time, status, and job id", async () => {
    mockListPeriodicTasks.mockResolvedValue([task()]);
    mockListHistory.mockResolvedValue([historyEntry("succeeded")]);
    renderWithProviders(<PeriodicTasks />);

    fireEvent.click(await screen.findByRole("button", { name: "履歴" }));

    const dialog = await screen.findByRole("dialog", { name: "実行履歴: 朝の要約" });
    expect(mockListHistory).toHaveBeenCalledWith("42", 20);
    // The history query resolves asynchronously after the modal mounts.
    expect(await within(dialog).findByText("成功")).toBeInTheDocument();
    expect(within(dialog).getByText(/job 12345/)).toBeInTheDocument();
  });

  it("cancels via confirmation with 停止中… busy label and not 削除中…", async () => {
    mockListPeriodicTasks.mockResolvedValue([task()]);
    mockListStatuses.mockResolvedValue([summary("running")]);
    // Hold the cancel promise open so the busy label is observable.
    let resolveCancel: () => void = () => {};
    mockCancel.mockReturnValue(
      new Promise<void>((resolve) => {
        resolveCancel = resolve;
      }),
    );
    renderWithProviders(<PeriodicTasks />);

    fireEvent.click(await screen.findByRole("button", { name: "停止" }));
    const dialog = screen.getByRole("dialog", { name: "実行を停止します" });
    fireEvent.click(within(dialog).getByRole("button", { name: "停止" }));

    expect(await within(dialog).findByText("停止中…")).toBeInTheDocument();
    expect(within(dialog).queryByText("削除中…")).not.toBeInTheDocument();
    await waitFor(() => expect(mockCancel).toHaveBeenCalledWith("777"));
    resolveCancel();
  });

  it("keeps unsupported schedulers' history available", async () => {
    mockListPeriodicTasks.mockResolvedValue([
      task({ id: "99", name: "旧形式", status: "unsupported", task: null }),
    ]);
    mockListHistory.mockResolvedValue([historyEntry("failed")]);
    renderWithProviders(<PeriodicTasks />);

    fireEvent.click(await screen.findByRole("button", { name: "履歴" }));
    expect(await screen.findByRole("dialog", { name: "実行履歴: 旧形式" })).toBeInTheDocument();
    expect(mockListHistory).toHaveBeenCalledWith("99", 20);
  });

  it("uses a sorted stable scheduler id list for the status query key", async () => {
    mockListPeriodicTasks.mockResolvedValue([
      task({ id: "3" }),
      task({ id: "1", name: "b" }),
      task({ id: "2", name: "c" }),
    ]);
    renderWithProviders(<PeriodicTasks />);

    await screen.findByText("朝の要約");
    await waitFor(() => expect(mockListStatuses).toHaveBeenCalled());
    // Query key ids are deduped + ascending-sorted regardless of card order.
    expect(mockListStatuses).toHaveBeenCalledWith(["1", "2", "3"]);
  });
});
