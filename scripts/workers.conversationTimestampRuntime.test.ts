import { describe, expect, it } from "vitest";
import personality from "../workers/lang-workers/workers/personality/thread-personality-single.yaml?raw";
import thread from "../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml?raw";
import reflection from "../workers/lang-workers/workers/thread-reflection/thread-reflection-single.yaml?raw";
import { runStep } from "./lib/workflowJq";

// Derived memories carry the source conversation's time, not the time they
// were written: daily rollups bucket by it and re-extraction compares it.
// The source thread's createdAt/updatedAt are server audit times.
const jsonBody = (workflow: string, step: string, bindings: Record<string, unknown>) =>
  JSON.parse(runStep(workflow, step, "json_body", bindings) as string);

const sourceThread = {
  data: { firstMessageAt: "100", lastMessageAt: "200", createdAt: "900", updatedAt: "999" },
};

describe("thread summary timestamps", () => {
  const writeBindings = {
    summary_json: { title: "t", summary: "s" },
    summary_metadata: "{}",
    thread: sourceThread,
    memory_thread_id: "7",
    summary_external_id: "summary:5",
    workflow: { input: { thread_id: "5", user_id: 1 } },
  };

  it("dates a new summary by the conversation", () => {
    const { memory } = jsonBody(thread, "addSummaryMemory", writeBindings);
    expect([memory.created_at, memory.updated_at]).toEqual(["100", "200"]);
  });

  it("keeps a re-summarized memory on the conversation's last message time", () => {
    const body = jsonBody(thread, "updateSummary", {
      ...writeBindings,
      existing_summary_memory: { id: { value: "9" }, data: { updatedAt: "100" } },
    });
    expect(body.data.updatedAt).toBe("200");
  });
});

describe("personality signal timestamps", () => {
  const input = { thread_id: "5", user_id: 1, force_reextract: false };
  const signal = { content: "{}", extractedAt: 0 };
  const writeBindings = {
    workflow: { input },
    thread: sourceThread,
    personality_json: { no_signal: false },
    truncation_level: 0,
    effective_personality_user_id: 1,
    memory_thread_id: "7",
  };

  it("re-extracts only when the conversation has a newer message", () => {
    const eligible = (updatedAt: string) =>
      runStep(personality, "checkEligibility", "eligible", {
        workflow: { input },
        thread: sourceThread,
        existing_signal: { data: { ...signal, updatedAt } },
      });
    expect(eligible("200")).toBe(false);
    expect(eligible("199")).toBe(true);
  });

  it("dates a new signal and its thread by the conversation", () => {
    const { memory } = jsonBody(personality, "addSignalMemory", writeBindings);
    expect([memory.created_at, memory.updated_at]).toEqual(["100", "200"]);
    const created = jsonBody(personality, "createMemoryThread", writeBindings);
    expect([created.created_at, created.updated_at]).toEqual(["100", "200"]);
  });

  it("keeps an updated signal on the conversation's last message time", () => {
    const body = jsonBody(personality, "updateSignal", {
      ...writeBindings,
      existing_signal: { id: { value: "9" }, data: { ...signal, updatedAt: "150" } },
    });
    expect(body.data.updatedAt).toBe("200");
  });
});

describe.each([
  { name: "thread summary", workflow: thread, label: "- startedAt: {{ conversation_started_at }}" },
  { name: "personality", workflow: personality, label: "- startedAt: {{ conversation_started_at }}" },
  { name: "reflection", workflow: reflection, label: "- Started: {{ conversation_started_at }}" },
])("$name prompt conversation start", ({ workflow, label }) => {
  const started = (firstMessageAt: unknown) =>
    runStep(workflow, "resolveConversationStart", "conversation_started_at", {
      thread: { data: { ...sourceThread.data, firstMessageAt } },
    });

  it("formats the conversation's first message time", () => {
    expect(started("1790638190243")).toBe("2026-09-28T23:29:50Z");
  });

  it("puts the conversation start, not the thread's audit time, in the prompt", () => {
    expect(workflow).toContain(label);
    expect(workflow).not.toContain("{{ thread.data.createdAt }}");
  });

  it("says unknown for a thread without messages", () => {
    expect(started(null)).toBe("unknown");
  });
});
