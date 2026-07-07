import { describe, expect, it } from "vitest";
import dailyWorkflow from "../workers/lang-workers/workers/daily-work-summary/daily-work-summary-single.yaml?raw";
import monthlyWorkflow from "../workers/lang-workers/workers/monthly-work-summary/monthly-work-summary-single.yaml?raw";
import weeklyWorkflow from "../workers/lang-workers/workers/weekly-work-summary/weekly-work-summary-single.yaml?raw";
import pipelineWorkflow from "../workers/workflows/agent-chat-pipeline/agent-chat-pipeline.yaml?raw";
import periodicWorkflow from "../workers/workflows/lookback-periodic-run.yaml?raw";
import monthlyBatchWorkflow from "../workers/workflows/monthly-work-summary/monthly-work-summary-batch.yaml?raw";

function yamlExpression(body: string): string {
  return `"${"$"}{ ${body} }"`;
}

describe("summary workflow timezone boundaries", () => {
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

  it("guards candidate offsets in monthly and import workflows too", () => {
    for (const yaml of [
      monthlyWorkflow,
      monthlyBatchWorkflow,
      pipelineWorkflow,
      periodicWorkflow,
    ]) {
      expect(yaml).toContain("candidate_epoch");
      expect(yaml).not.toContain("$e - ($b - $e)");
    }
    expect(monthlyWorkflow).toContain('candidate_epoch | strflocaltime("%Y-%m-%d")');
    expect(monthlyBatchWorkflow).toContain('candidate_epoch | strflocaltime("%Y-%m-%d")');
    expect(pipelineWorkflow).toContain('candidate_epoch | strflocaltime("%Y-%m-%d")');
    expect(pipelineWorkflow).toContain("candidate_epoch | strflocaltime");
    expect(periodicWorkflow).toContain("candidate_epoch | strflocaltime");
  });
});
