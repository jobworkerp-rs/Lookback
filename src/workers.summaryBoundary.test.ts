import { describe, expect, it } from "vitest";
import dailyWorkflow from "../workers/lang-workers/workers/daily-work-summary/daily-work-summary-single.yaml?raw";
import monthlyWorkflow from "../workers/lang-workers/workers/monthly-work-summary/monthly-work-summary-single.yaml?raw";
import threadSummarySingleWorkflow from "../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml?raw";
import weeklyWorkflow from "../workers/lang-workers/workers/weekly-work-summary/weekly-work-summary-single.yaml?raw";
import periodicWorkflow from "../workers/workflows/lookback-periodic-run.yaml?raw";
import monthlyBatchWorkflow from "../workers/workflows/monthly-work-summary/monthly-work-summary-batch.yaml?raw";
import threadPersonalityBatchWorkflow from "../workers/workflows/personality/thread-personality-batch.yaml?raw";
import threadReflectionBatchWorkflow from "../workers/workflows/thread-reflection/thread-reflection-batch.yaml?raw";
import threadSummaryBatchWorkflow from "../workers/workflows/thread-summary/thread-summary-batch.yaml?raw";

function yamlExpression(body: string): string {
  return `"${"$"}{ ${body} }"`;
}

function needsThreadSummary(
  thread: { lastMessageAt: string; updatedAt: string },
  existingSummary: { updatedAt: string },
): boolean {
  return BigInt(thread.lastMessageAt) > BigInt(existingSummary.updatedAt);
}

function sortPersonalityThreadsNewestFirst<
  T extends { id: { value: string }; data: { lastMessageAt: string; updatedAt: string } },
>(threads: T[]): T[] {
  return [...threads].sort((left, right) => {
    const timeOrder = BigInt(right.data.lastMessageAt) - BigInt(left.data.lastMessageAt);
    if (timeOrder !== 0n) return timeOrder > 0n ? 1 : -1;
    const idOrder = BigInt(right.id.value) - BigInt(left.id.value);
    return idOrder === 0n ? 0 : idOrder > 0n ? 1 : -1;
  });
}

describe("summary workflow timezone boundaries", () => {
  it("uses conversation-time windows for manual thread batches", () => {
    for (const yaml of [
      threadSummaryBatchWorkflow,
      threadPersonalityBatchWorkflow,
      threadReflectionBatchWorkflow,
    ]) {
      expect(yaml).toContain("last_message_after");
      expect(yaml).toContain("last_message_within_hours");
      expect(yaml).not.toContain("updated_within_hours");
      expect(yaml).not.toContain("resolveUpdatedAfter");
    }
  });

  it("treats a present empty import ID set as zero targets", () => {
    for (const yaml of [
      threadSummaryBatchWorkflow,
      threadPersonalityBatchWorkflow,
      threadReflectionBatchWorkflow,
    ]) {
      expect(yaml).toContain("target_thread_ids");
      expect(yaml).toContain('has("target_thread_ids")');
      expect(yaml).toContain("thread_ids");
    }
  });

  it("pre-skips thread summaries by conversation freshness, not audit updates", () => {
    expect(threadSummaryBatchWorkflow).toContain("$thread.data.lastMessageAt");
    expect(threadSummaryBatchWorkflow).not.toContain(
      "$thread.data.updatedAt\n                          > $existing_summary.memory.data.updatedAt",
    );
    expect(
      needsThreadSummary({ lastMessageAt: "100", updatedAt: "999" }, { updatedAt: "100" }),
    ).toBe(false);
    expect(
      needsThreadSummary({ lastMessageAt: "101", updatedAt: "101" }, { updatedAt: "100" }),
    ).toBe(true);
  });

  it("sorts personality targets by exact last-message time with a stable ID tie-break", () => {
    expect(threadPersonalityBatchWorkflow).toContain(".data.lastMessageAt");
    expect(threadPersonalityBatchWorkflow).not.toContain(
      "sort_by((.data.updatedAt // 0) | tonumber)",
    );
    const sorted = sortPersonalityThreadsNewestFirst([
      {
        id: { value: "7" },
        data: { lastMessageAt: "9007199254740992", updatedAt: "9999999999999999" },
      },
      {
        id: { value: "8" },
        data: { lastMessageAt: "9007199254740993", updatedAt: "1" },
      },
      {
        id: { value: "9" },
        data: { lastMessageAt: "9007199254740993", updatedAt: "0" },
      },
    ]);
    expect(sorted.map((thread) => thread.id.value)).toEqual(["9", "8", "7"]);
  });

  it("timestamps thread-summary derived data from source message extrema", () => {
    expect(threadSummarySingleWorkflow).toContain("created_at: $thread.data.firstMessageAt");
    expect(threadSummarySingleWorkflow).toContain("updated_at: $thread.data.lastMessageAt");
    expect(threadSummarySingleWorkflow).not.toContain("created_at: $thread.data.createdAt");
    expect(threadSummarySingleWorkflow).not.toContain("updated_at: $thread.data.updatedAt");
  });

  it("computes daily end from the next local midnight instead of a fixed 24h add", () => {
    expect(dailyWorkflow).not.toContain(
      `day_end_ms: ${yamlExpression("$day_start_ms + 86400000")}`,
    );
    expect(dailyWorkflow).toContain("computeDayEnd");
    expect(dailyWorkflow).toContain('$target_date_resolved + "T00:00:00"');
    expect(dailyWorkflow).toContain("next_day");
  });

  it("keeps same-date candidate daily boundaries before re-evaluating offsets", () => {
    expect(dailyWorkflow).toContain("candidate_epoch");
    expect(dailyWorkflow).toContain('candidate_epoch | strflocaltime("%Y-%m-%d")');
    expect(dailyWorkflow).toContain("then $candidate_epoch");
    expect(dailyWorkflow).not.toContain("$e - ($b - $e)");
  });

  it("computes weekly end from the next Monday local midnight instead of a fixed 7-day add", () => {
    expect(weeklyWorkflow).not.toContain(
      `week_end_ms: ${yamlExpression("$week_start_ms + 7 * 86400000")}`,
    );
    expect(weeklyWorkflow).toContain("computeWeekEnd");
    expect(weeklyWorkflow).toContain("next_week");
  });

  it("keeps same-date candidate weekly boundaries before re-evaluating offsets", () => {
    expect(weeklyWorkflow).toContain("candidate_epoch");
    expect(weeklyWorkflow).toContain('candidate_epoch | strflocaltime("%Y-%m-%d")');
    expect(weeklyWorkflow).toContain("then $candidate_epoch");
    expect(weeklyWorkflow).not.toContain("$e - ($b - $e)");
  });

  it("guards candidate offsets in monthly and periodic import workflows too", () => {
    for (const yaml of [monthlyWorkflow, monthlyBatchWorkflow, periodicWorkflow]) {
      expect(yaml).toContain("candidate_epoch");
      expect(yaml).not.toContain("$e - ($b - $e)");
    }
    expect(monthlyWorkflow).toContain('candidate_epoch | strflocaltime("%Y-%m-%d")');
    expect(monthlyBatchWorkflow).toContain('candidate_epoch | strflocaltime("%Y-%m-%d")');
    expect(periodicWorkflow).toContain("candidate_epoch | strflocaltime");
  });
});
