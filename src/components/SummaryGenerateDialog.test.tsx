import { fireEvent, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { SidecarStatus } from "@/hooks/useSidecarStatus";
import i18n from "@/i18n";
import { localDateToEpochMs } from "@/lib/dateInput";
import type { KindSelection } from "@/lib/summaryKind";
import { renderWithProviders } from "@/test-utils";
import { buildGenerateRequest, SummaryGenerateDialog, targetRange } from "./SummaryGenerateDialog";

const generateSummaries = vi.fn();
vi.mock("@/api", () => ({
  generateSummaries: (req: unknown) => generateSummaries(req),
}));

const READY: SidecarStatus = { phase: "ready", warnings: [] };

function setup(initialKind: Parameters<typeof SummaryGenerateDialog>[0]["initialKind"]) {
  const onStart = vi.fn();
  const onDispatchError = vi.fn();
  const onClose = vi.fn();
  renderWithProviders(
    <SummaryGenerateDialog
      onClose={onClose}
      onStart={onStart}
      onDispatchError={onDispatchError}
      initialKind={initialKind}
      sidecar={READY}
    />,
  );
  return { onStart, onDispatchError, onClose };
}

beforeEach(() => {
  i18n.changeLanguage("ja");
  generateSummaries.mockReset().mockResolvedValue({ job_id_hint: "summaries-1" });
});

function sel(partial: Partial<KindSelection>): KindSelection {
  return {
    "per-thread": false,
    "thread-group": false,
    daily: false,
    weekly: false,
    monthly: false,
    ...partial,
  };
}

describe("buildGenerateRequest", () => {
  it("returns no request and no error when nothing is selected", () => {
    expect(buildGenerateRequest(sel({}), "", "", 9)).toEqual({ request: null, error: null });
  });

  it("per-thread only + no range stays unbounded (omits epoch bounds)", () => {
    const { request, error } = buildGenerateRequest(sel({ "per-thread": true }), "", "", 9);
    expect(error).toBeNull();
    expect(request?.run_per_thread).toBe(true);
    expect(request?.last_message_after_ms).toBeUndefined();
    expect(request?.last_message_before_ms).toBeUndefined();
    expect(request?.daily_start).toBe("");
  });

  it("requires a period for a ThreadGroup-only run", () => {
    expect(
      buildGenerateRequest(sel({ "per-thread": true, "thread-group": true }), "", "", 9),
    ).toEqual({
      request: null,
      error: "required",
    });
  });

  it("limits a ThreadGroup run to groups active in the selected days (exclusive bounds)", () => {
    const { request, error } = buildGenerateRequest(
      sel({ "per-thread": true, "thread-group": true }),
      "2026-10-05",
      "2026-10-11",
      9,
    );
    expect(error).toBeNull();
    expect(request).toMatchObject({
      run_thread_group: true,
      run_daily: false,
      thread_group_activity_after_ms: (localDateToEpochMs("2026-10-05") as number) - 1,
      thread_group_activity_before_ms: localDateToEpochMs("2026-10-12"),
    });
  });

  it("scopes the ThreadGroup stage of a period run to the same day span", () => {
    const { request } = buildGenerateRequest(
      sel({ "per-thread": true, "thread-group": true, daily: true }),
      "2026-10-05",
      "2026-10-11",
      9,
    );
    expect(request?.thread_group_activity_after_ms).toBe(request?.last_message_after_ms);
    expect(request?.thread_group_activity_before_ms).toBe(
      (request?.last_message_before_ms as number) + 1,
    );
  });

  it("omits the ThreadGroup window when ThreadGroup is not selected", () => {
    const { request } = buildGenerateRequest(
      sel({ "per-thread": true, daily: true }),
      "2026-10-05",
      "2026-10-11",
      9,
    );
    expect(request?.thread_group_activity_after_ms).toBeUndefined();
    expect(request?.thread_group_activity_before_ms).toBeUndefined();
  });

  it("rejects a one-sided range", () => {
    expect(buildGenerateRequest(sel({ daily: true }), "2026-05-01", "", 9)).toEqual({
      request: null,
      error: "one-sided",
    });
  });

  it("rejects a reversed range", () => {
    expect(buildGenerateRequest(sel({ daily: true }), "2026-05-31", "2026-05-01", 9)).toEqual({
      request: null,
      error: "reversed",
    });
  });

  it("daily range fills all layers below from the same day span", () => {
    const { request } = buildGenerateRequest(
      sel({ "per-thread": true, daily: true }),
      "2026-05-01",
      "2026-05-31",
      9,
    );
    expect(request?.daily_start).toBe("2026-05-01");
    expect(request?.daily_end).toBe("2026-05-31");
    expect(request?.last_message_after_ms).toBeDefined();
    expect(request?.last_message_before_ms).toBeDefined();
  });

  it("weekly date range aggregates every week containing either selected date", () => {
    const { request, error } = buildGenerateRequest(
      sel({ "per-thread": true, daily: true, weekly: true }),
      "2026-07-22",
      "2026-07-28",
      9,
    );
    expect(error).toBeNull();
    expect(request).toMatchObject({
      run_per_thread: true,
      run_daily: true,
      run_weekly: true,
      run_monthly: false,
      weekly_start: "2026-W30",
      weekly_end: "2026-W31",
      daily_start: "2026-07-20",
      daily_end: "2026-08-02",
    });
  });

  it("rejects a reverse-ordered weekly date range in the same ISO week", () => {
    expect(
      buildGenerateRequest(
        sel({ "per-thread": true, daily: true, weekly: true }),
        "2026-07-26",
        "2026-07-20",
        9,
      ),
    ).toEqual({ request: null, error: "reversed" });
  });

  it("weekly date range derives ISO weeks across the calendar-year boundary", () => {
    const { request, error } = buildGenerateRequest(
      sel({ "per-thread": true, daily: true, weekly: true }),
      "2025-12-31",
      "2026-01-01",
      9,
    );
    expect(error).toBeNull();
    expect(request).toMatchObject({
      weekly_start: "2026-W01",
      weekly_end: "2026-W01",
      daily_start: "2025-12-29",
      daily_end: "2026-01-04",
    });
  });

  it("rejects malformed weekly calendar dates", () => {
    expect(buildGenerateRequest(sel({ weekly: true }), "2026-02-30", "2026-03-01", 9)).toEqual({
      request: null,
      error: "invalid",
    });
  });

  it("monthly range extends daily START to the leading week's Monday", () => {
    const { request } = buildGenerateRequest(
      sel({ "per-thread": true, daily: true, weekly: true, monthly: true }),
      "2026-03",
      "2026-05",
      9,
    );
    // Monthly/weekly stay the selected range.
    expect(request?.monthly_start).toBe("2026-03");
    expect(request?.monthly_end).toBe("2026-05");
    expect(request?.weekly_start).toBe("2026-W09");
    expect(request?.weekly_end).toBe("2026-W22");
    // Daily start backs up to the leading week's Monday: 2026-03-01 (Sun) -> 02-23.
    expect(request?.daily_start).toBe("2026-02-23");
    expect(request?.daily_end).toBe("2026-05-31");
    // per-thread epoch bounds follow the extended daily span.
    expect(request?.last_message_after_ms).toBe((localDateToEpochMs("2026-02-23") as number) - 1);
  });

  it("monthly range keeps the daily END inside the month (boundary-week attribution)", () => {
    // 2026-04 ends on Thu 04-30. The trailing week (W18) must NOT extend to
    // 05-03, or its updated_at lands in May and April's monthly drops it.
    const { request } = buildGenerateRequest(
      sel({ "per-thread": true, daily: true, weekly: true, monthly: true }),
      "2026-04",
      "2026-04",
      9,
    );
    expect(request?.monthly_start).toBe("2026-04");
    expect(request?.monthly_end).toBe("2026-04");
    expect(request?.daily_start).toBe("2026-03-30"); // W14 Monday
    expect(request?.daily_end).toBe("2026-04-30"); // unchanged, stays in April
  });

  it("no-range period run falls back to the previous period (daily=yesterday)", () => {
    const { request } = buildGenerateRequest(sel({ "per-thread": true, daily: true }), "", "", 9);
    expect(request?.daily_start).not.toBe("");
    expect(request?.daily_start).toBe(request?.daily_end); // exactly one day
  });
});

describe("targetRange", () => {
  const request = (top: Parameters<typeof targetRange>[0], from: string, to: string) => {
    const selection = sel({
      "per-thread": true,
      "thread-group": top !== "per-thread",
      daily: top !== "per-thread",
      weekly: top === "weekly" || top === "monthly",
      monthly: top === "monthly",
    });
    return buildGenerateRequest(selection, from, to, 9, "Asia/Tokyo").request;
  };

  it("names the selected period of the top granularity", () => {
    expect(targetRange("monthly", request("monthly", "2026-07", "2026-09"), "", "")).toBe(
      "2026-07〜2026-09",
    );
    expect(targetRange("weekly", request("weekly", "2026-09-02", "2026-09-16"), "", "")).toBe(
      "2026-W36〜2026-W38",
    );
    expect(targetRange("daily", request("daily", "2026-09-01", "2026-09-01"), "", "")).toBe(
      "2026-09-01",
    );
  });

  it("names a per-thread run by its days, and nothing when unbounded", () => {
    const bounded = request("per-thread", "2026-09-01", "2026-09-03");
    expect(targetRange("per-thread", bounded, "2026-09-01", "2026-09-03")).toBe(
      "2026-09-01〜2026-09-03",
    );
    expect(targetRange("per-thread", request("per-thread", "", ""), "", "")).toBeNull();
  });
});

describe("SummaryGenerateDialog", () => {
  it("turning on weekly auto-selects the finer layers (dependency)", () => {
    setup("daily");
    fireEvent.click(screen.getByLabelText("週次"));
    expect((screen.getByLabelText("週次") as HTMLInputElement).checked).toBe(true);
    expect((screen.getByLabelText("日次") as HTMLInputElement).checked).toBe(true);
    expect(screen.queryByLabelText("スレッド")).not.toBeInTheDocument();
    expect((screen.getByLabelText("月次") as HTMLInputElement).checked).toBe(false);
  });

  it("turning off daily clears the coarser layers", () => {
    setup("monthly"); // selects all four
    fireEvent.click(screen.getByLabelText("日次"));
    expect((screen.getByLabelText("日次") as HTMLInputElement).checked).toBe(false);
    expect((screen.getByLabelText("週次") as HTMLInputElement).checked).toBe(false);
    expect((screen.getByLabelText("月次") as HTMLInputElement).checked).toBe(false);
    expect(screen.queryByLabelText("スレッド")).not.toBeInTheDocument();
  });

  it("disables generation when ThreadGroup and every public stage are deselected", () => {
    setup("daily");

    fireEvent.click(screen.getByLabelText("スレッドグループ"));

    const submit = screen.getByText("生成を開始");
    expect(submit).toBeDisabled();
    fireEvent.click(submit);
    expect(generateSummaries).not.toHaveBeenCalled();
  });

  it("uses an app-drawn month picker when monthly is the top granularity", () => {
    const { container } = renderWith("monthly");
    expect(container.querySelector('input[type="month"]')).toBeNull();
    fireEvent.click(screen.getByLabelText("開始"));
    expect(screen.getByRole("button", { name: "7月" })).toBeInTheDocument();
  });

  it("sends the months picked in the month pickers", async () => {
    vi.useFakeTimers({ now: new Date(2026, 9, 16), toFake: ["Date"] });
    renderWith("monthly");

    fireEvent.click(screen.getByLabelText("開始"));
    fireEvent.click(screen.getByRole("button", { name: "7月" }));
    fireEvent.click(screen.getByLabelText("終了"));
    fireEvent.click(screen.getByRole("button", { name: "9月" }));
    expect(screen.getByText("対象：2026-07〜2026-09")).toBeInTheDocument();

    fireEvent.click(screen.getByText("生成を開始"));
    await vi.waitFor(() => expect(generateSummaries).toHaveBeenCalledTimes(1));
    const req = generateSummaries.mock.calls[0]?.[0];
    expect([req.monthly_start, req.monthly_end]).toEqual(["2026-07", "2026-09"]);
    vi.useRealTimers();
  });

  it("states the fallback period in the same form when no range is picked", () => {
    vi.useFakeTimers({ now: new Date(2026, 9, 16), toFake: ["Date"] });
    renderWith("monthly");
    expect(screen.getByText("対象：2026-09")).toBeInTheDocument();
    vi.useRealTimers();
  });

  it("uses a date picker when weekly is the top granularity", () => {
    renderWith("weekly");
    fireEvent.click(screen.getByLabelText("開始"));
    expect(screen.getAllByText("月")[0]).toBeInTheDocument();
    expect(screen.getAllByText("日")[0]).toBeInTheDocument();
    expect(document.querySelector('input[type="week"]')).toBeNull();
  });

  it("uses a date picker when daily is the top granularity", () => {
    renderWith("daily");
    expect(screen.getByLabelText("開始")).toBeInTheDocument();
    expect(document.querySelector('input[type="month"]')).toBeNull();
  });

  it("resets the staged slot with the dispatch id before invoking generate_summaries", async () => {
    const order: string[] = [];
    const onStart = vi.fn((dispatchId: string) => {
      order.push(`reset:${dispatchId}`);
    });
    const onDispatchError = vi.fn();
    generateSummaries.mockImplementationOnce(async (req: { dispatch_id: string }) => {
      order.push(`invoke:${req.dispatch_id}`);
      return { job_id_hint: "worker-job" };
    });
    renderWithProviders(
      <SummaryGenerateDialog
        onClose={vi.fn()}
        onStart={onStart}
        onDispatchError={onDispatchError}
        initialKind="daily"
        sidecar={READY}
      />,
    );

    expect((screen.getByLabelText("エラー時に中断する") as HTMLInputElement).checked).toBe(false);
    fireEvent.click(screen.getByLabelText("エラー時に中断する"));
    fireEvent.click(screen.getByText("生成を開始"));
    await vi.waitFor(() => expect(generateSummaries).toHaveBeenCalledTimes(1));
    const req = generateSummaries.mock.calls[0]?.[0];
    expect(req.dispatch_id).toEqual(expect.any(String));
    expect(onStart).toHaveBeenCalledWith(req.dispatch_id);
    expect(order).toEqual([`reset:${req.dispatch_id}`, `invoke:${req.dispatch_id}`]);
    expect(req.run_daily).toBe(true);
    expect(req.run_per_thread).toBe(true); // daily depends on per-thread
    expect(req.run_thread_group).toBe(true); // periods depend on thread-group
    expect(req.stop_on_error).toBe(true);
  });

  it("releases the pre-reset slot when dispatch fails", async () => {
    const { onDispatchError } = setup("daily");
    generateSummaries.mockRejectedValueOnce(new Error("dispatch unavailable"));

    fireEvent.click(screen.getByText("生成を開始"));

    await screen.findByText("dispatch unavailable");
    const req = generateSummaries.mock.calls[0]?.[0];
    expect(onDispatchError).toHaveBeenCalledWith(req.dispatch_id);
  });

  it("turning off ThreadGroup clears public periods but keeps the implicit thread stage", () => {
    setup("daily");

    fireEvent.click(screen.getByLabelText("スレッドグループ"));

    expect((screen.getByLabelText("スレッドグループ") as HTMLInputElement).checked).toBe(false);
    expect((screen.getByLabelText("日次") as HTMLInputElement).checked).toBe(false);
    expect((screen.getByLabelText("週次") as HTMLInputElement).checked).toBe(false);
    expect((screen.getByLabelText("月次") as HTMLInputElement).checked).toBe(false);
    expect(screen.queryByText("対象期間")).not.toBeInTheDocument();
    expect(screen.getByText("生成を開始")).toBeDisabled();
  });

  it("asks for a period when ThreadGroup is the only selected stage", () => {
    setup("daily");
    fireEvent.click(screen.getByLabelText("日次"));

    expect((screen.getByLabelText("スレッドグループ") as HTMLInputElement).checked).toBe(true);
    expect(screen.getByText("対象期間")).toBeInTheDocument();
    expect(screen.getByText("期間を指定してください。")).toBeInTheDocument();
    expect(screen.getByText("生成を開始")).toBeDisabled();
  });

  it("disables the submit button for a one-sided range", () => {
    renderWith("daily");
    fireEvent.click(screen.getByLabelText("開始"));
    fireEvent.click(screen.getByRole("button", { name: String(new Date().getDate()) }));
    expect(screen.getByText("生成を開始")).toBeDisabled();
  });

  it("disables generation when the LLM plugin failed to init", () => {
    const down: SidecarStatus = {
      phase: "ready",
      warnings: [{ kind: "worker_apply_failed", message: "x", detail: null }],
    };
    renderWithProviders(
      <SummaryGenerateDialog
        onClose={vi.fn()}
        onStart={vi.fn()}
        onDispatchError={vi.fn()}
        initialKind="daily"
        sidecar={down}
      />,
    );
    expect(screen.getByText("生成を開始")).toBeDisabled();
  });
});

function renderWith(initialKind: Parameters<typeof SummaryGenerateDialog>[0]["initialKind"]) {
  return renderWithProviders(
    <SummaryGenerateDialog
      onClose={vi.fn()}
      onStart={vi.fn()}
      onDispatchError={vi.fn()}
      initialKind={initialKind}
      sidecar={READY}
    />,
  );
}
