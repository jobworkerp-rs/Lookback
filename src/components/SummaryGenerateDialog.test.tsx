import { fireEvent, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { SidecarStatus } from "@/hooks/useSidecarStatus";
import i18n from "@/i18n";
import { localDateToEpochMs } from "@/lib/dateInput";
import type { KindSelection } from "@/lib/summaryKind";
import { renderWithProviders } from "@/test-utils";
import { buildGenerateRequest, SummaryGenerateDialog } from "./SummaryGenerateDialog";

const generateSummaries = vi.fn();
vi.mock("@/api", () => ({
  generateSummaries: (req: unknown) => generateSummaries(req),
}));

const READY: SidecarStatus = { phase: "ready", warnings: [] };

function setup(initialKind: Parameters<typeof SummaryGenerateDialog>[0]["initialKind"]) {
  const onStarted = vi.fn();
  const onClose = vi.fn();
  renderWithProviders(
    <SummaryGenerateDialog
      onClose={onClose}
      onStarted={onStarted}
      initialKind={initialKind}
      sidecar={READY}
    />,
  );
  return { onStarted, onClose };
}

beforeEach(() => {
  i18n.changeLanguage("ja");
  generateSummaries.mockReset().mockResolvedValue({ job_id_hint: "summaries-1" });
});

function sel(partial: Partial<KindSelection>): KindSelection {
  return { "per-thread": false, daily: false, weekly: false, monthly: false, ...partial };
}

describe("buildGenerateRequest", () => {
  it("returns no request and no error when nothing is selected", () => {
    expect(buildGenerateRequest(sel({}), "", "", 9)).toEqual({ request: null, error: null });
  });

  it("per-thread only + no range stays unbounded (omits epoch bounds)", () => {
    const { request, error } = buildGenerateRequest(sel({ "per-thread": true }), "", "", 9);
    expect(error).toBeNull();
    expect(request?.run_per_thread).toBe(true);
    expect(request?.updated_after_ms).toBeUndefined();
    expect(request?.updated_before_ms).toBeUndefined();
    expect(request?.daily_start).toBe("");
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
    expect(request?.updated_after_ms).toBeDefined();
    expect(request?.updated_before_ms).toBeDefined();
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
    expect(request?.updated_after_ms).toBe((localDateToEpochMs("2026-02-23") as number) - 1);
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

describe("SummaryGenerateDialog", () => {
  it("turning on weekly auto-selects the finer layers (dependency)", () => {
    setup("per-thread");
    fireEvent.click(screen.getByLabelText("週次"));
    expect((screen.getByLabelText("週次") as HTMLInputElement).checked).toBe(true);
    expect((screen.getByLabelText("日次") as HTMLInputElement).checked).toBe(true);
    expect((screen.getByLabelText("スレッド") as HTMLInputElement).checked).toBe(true);
    expect((screen.getByLabelText("月次") as HTMLInputElement).checked).toBe(false);
  });

  it("turning off daily clears the coarser layers", () => {
    setup("monthly"); // selects all four
    fireEvent.click(screen.getByLabelText("日次"));
    expect((screen.getByLabelText("日次") as HTMLInputElement).checked).toBe(false);
    expect((screen.getByLabelText("週次") as HTMLInputElement).checked).toBe(false);
    expect((screen.getByLabelText("月次") as HTMLInputElement).checked).toBe(false);
    expect((screen.getByLabelText("スレッド") as HTMLInputElement).checked).toBe(true);
  });

  it("uses a month picker when monthly is the top granularity", () => {
    const { container } = renderWith("monthly");
    expect(container.querySelector('input[type="month"]')).toBeTruthy();
    expect(container.querySelector('input[type="week"]')).toBeNull();
  });

  it("uses a week picker when weekly is the top granularity", () => {
    const { container } = renderWith("weekly");
    expect(container.querySelector('input[type="week"]')).toBeTruthy();
  });

  it("uses a date picker when daily is the top granularity", () => {
    const { container } = renderWith("daily");
    expect(container.querySelector('input[type="date"]')).toBeTruthy();
    expect(container.querySelector('input[type="month"]')).toBeNull();
  });

  it("dispatches generate_summaries with the staged request", async () => {
    const { onStarted } = setup("daily");
    fireEvent.click(screen.getByText("生成を開始"));
    await vi.waitFor(() => expect(generateSummaries).toHaveBeenCalledTimes(1));
    const req = generateSummaries.mock.calls[0]?.[0];
    expect(req.run_daily).toBe(true);
    expect(req.run_per_thread).toBe(true); // daily depends on per-thread
    await vi.waitFor(() => expect(onStarted).toHaveBeenCalledWith("summaries-1"));
  });

  it("disables the submit button for a one-sided range", () => {
    const { container } = renderWith("daily");
    const dateInput = container.querySelector('input[type="date"]') as HTMLInputElement;
    fireEvent.change(dateInput, { target: { value: "2026-05-01" } });
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
        onStarted={vi.fn()}
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
      onStarted={vi.fn()}
      initialKind={initialKind}
      sidecar={READY}
    />,
  );
}
