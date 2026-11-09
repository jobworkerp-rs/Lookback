import { describe, expect, it } from "vitest";
import threadBatchWorkflow from "../workers/workflows/thread-summary/thread-summary-batch.yaml?raw";
import single from "../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml?raw";
import { runStep } from "./lib/workflowJq";

const eligible = (
  thread: Record<string, unknown>,
  existingSummaryUpdatedAt: unknown,
  forceResummarize = false,
) =>
  runStep(single, "checkEligibility", "eligible", {
    workflow: { input: { force_resummarize: forceResummarize } },
    thread: { data: { description: "summary", lastMessageAt: "1792000000000", ...thread } },
    existing_summary_memory:
      existingSummaryUpdatedAt === null ? null : { data: { updatedAt: existingSummaryUpdatedAt } },
  });

describe("thread-summary-single decides freshness before reading messages", () => {
  it("skips an up-to-date thread before fetching its messages", () => {
    const skip = single.indexOf("- skipIfNotEligible:");
    expect(skip).toBeGreaterThan(-1);
    expect(single.indexOf("- findMemoryThread:")).toBeLessThan(skip);
    expect(single.indexOf("- findExistingSummary:")).toBeLessThan(skip);
    expect(single.indexOf("- checkThreadExists:")).toBeLessThan(single.indexOf("- findMemoryThread:"));
    expect(skip).toBeLessThan(single.indexOf("- fetchMessages:"));
    expect(skip).toBeLessThan(single.indexOf("- checkMessageCount:"));
  });

  it("keeps a summary newer than the last message", () => {
    expect(eligible({}, "1792000000001")).toBe(false);
    expect(eligible({}, "1792000000000")).toBe(false);
  });

  it("re-summarizes a thread whose last message is newer than its summary", () => {
    expect(eligible({ lastMessageAt: "1792000000002" }, "1792000000001")).toBe(true);
  });

  it("compares int64 timestamps numerically even when their digit counts differ", () => {
    expect(eligible({ lastMessageAt: "10000000000000" }, "9999999999999")).toBe(true);
    expect(eligible({ lastMessageAt: "9999999999999" }, "10000000000000")).toBe(false);
    expect(eligible({ lastMessageAt: 1792000000005 }, "1792000000001")).toBe(true);
  });

  it("re-summarizes a thread without a description or an existing summary, or when forced", () => {
    expect(eligible({ description: "" }, "1792000000001")).toBe(true);
    expect(eligible({}, null)).toBe(true);
    expect(eligible({}, "1792000000001", true)).toBe(true);
  });
});

describe("thread-summary-batch leaves freshness to the single", () => {
  it("dispatches every filtered thread without a bulk summary lookup", () => {
    expect(threadBatchWorkflow).not.toContain("MemoryService/FindListByCondition");
    expect(threadBatchWorkflow).toContain(`in: "\${ $target_threads }"`);
  });

  it("targets the threads selected by the thread filter", () => {
    const all_threads = [{ id: { value: 1 } }, { id: { value: 2 } }, { id: { value: 3 } }];
    expect(
      runStep(threadBatchWorkflow, "filterThreads", "target_threads", {
        all_threads,
        workflow: { input: { target_thread_ids: ["1", "3"] } },
      }),
    ).toEqual([{ id: { value: 1 } }, { id: { value: 3 } }]);
    expect(
      runStep(threadBatchWorkflow, "filterThreads", "target_threads", {
        all_threads,
        workflow: { input: {} },
      }),
    ).toEqual(all_threads);
  });
});
