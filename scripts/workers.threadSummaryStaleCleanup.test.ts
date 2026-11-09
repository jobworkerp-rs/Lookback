import { describe, expect, it } from "vitest";
import single from "../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml?raw";
import { blockBetween, runStep } from "./lib/workflowJq";

const cleanup = () => blockBetween(single, "- cleanupStaleSummaries:", "- updateSummary:");

describe("thread-summary-single stale summary cleanup", () => {
  it("looks the stale summary up by its external_id instead of scanning every summary thread", () => {
    const block = cleanup();
    expect(block).toContain("/llm_memory.service.MemoryService/FindListByCondition");
    expect(block).not.toContain("FindThreadListByLabels");
    expect(block).not.toContain("FindMemoriesByThreadId");
  });

  it("keys the summary by its source thread for the lookup and both writes", () => {
    const key = (threadId: unknown) =>
      runStep(single, "resolveMemoryThreadId", "summary_external_id", {
        workflow: { input: { thread_id: threadId } },
      });
    expect(key("123")).toBe("summary:123");
    expect(key(123)).toBe("summary:123");
    expect(blockBetween(single, "- updateSummary:", "- addSummaryMemory:")).toContain(
      ".externalId = $summary_external_id",
    );
    expect(blockBetween(single, "- addSummaryMemory:", "- resolveSavedSummaryMemoryId:")).toContain(
      "external_id: $summary_external_id",
    );
  });

  it("requests only the summary with this thread's external_id", () => {
    const body = JSON.parse(
      runStep(single, "findStaleSummaries", "json_body", {
        workflow: { input: { user_id: 1 } },
        summary_external_id: "summary:123",
      }) as string,
    );
    expect(body).toEqual({
      user_id: { value: 1 },
      external_id: "summary:123",
      memory_kinds: ["MEMORY_KIND_THREAD_SUMMARY"],
    });
  });

  it("returns the matching memories and falls back to none for an empty or missing body", () => {
    const shape = (response: unknown) => runStep(single, "findStaleSummaries", "stale_summaries", {}, response);
    const entries = [
      { memory: { id: { value: "9" } }, threadId: { value: "5" } },
      { memory: { id: { value: "10" } }, threadId: { value: "6" } },
    ];
    expect(shape({ jsonBody: JSON.stringify(entries) })).toEqual([{ id: { value: "9" } }, { id: { value: "10" } }]);
    expect(shape({ jsonBody: "[]" })).toEqual([]);
    expect(shape({ jsonBody: "" })).toEqual([]);
    expect(shape({})).toEqual([]);
  });

  it("runs only when the summary is about to be added rather than updated in place", () => {
    const runs = (existing: unknown, existingThread: string | null, target: string) =>
      runStep(single, "cleanupStaleSummaries", "if", {
        existing_summary_memory: existing,
        existing_memory_thread_id: existingThread,
        memory_thread_id: target,
      });
    expect(runs(null, null, "7")).toBe(true);
    expect(runs(null, "7", "7")).toBe(true);
    expect(runs({ id: { value: "9" } }, "7", "7")).toBe(false);
  });

  it("deletes each matching memory by its id before the summary is added", () => {
    const deletion = blockBetween(cleanup(), "- deleteStale:");
    expect(deletion).toContain('in: "${ $stale_summaries }"');
    expect(deletion).toContain("/llm_memory.service.MemoryService/Delete");
    expect(deletion).toContain("${ $stale.id | tojson }");
    blockBetween(single, "- cleanupStaleSummaries:", "- addSummaryMemory:");
  });
});
