import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ChatStepUpdate } from "@/types/api";

const chatAskMock = vi.fn(async () => ({ job_id: "ignored" }));
const chatCancelMock = vi.fn(async (_jobId: string): Promise<void> => undefined);
vi.mock("@/api", () => ({
  chatAsk: () => chatAskMock(),
  chatCancel: (jobId: string) => chatCancelMock(jobId),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
}));

import { buildHistoryMessages, type ChatTurn, mergeChatEvent, useRagChat } from "./useRagChat";

function baseTurn(overrides: Partial<ChatTurn> = {}): ChatTurn {
  return {
    jobId: "chat-1",
    question: "what did we decide?",
    answer: "",
    sources: [],
    phase: "start",
    message: null,
    error: null,
    ...overrides,
  };
}

function ev(partial: Omit<ChatStepUpdate, "job_id">): ChatStepUpdate {
  return { job_id: "chat-1", ...partial };
}

describe("mergeChatEvent — TEST-CHAT-8", () => {
  it("appends a token delta to the matching turn's answer", () => {
    const turns = [baseTurn()];
    const next = mergeChatEvent(turns, ev({ phase: "token", token_delta: "Hello" }));
    expect(next[0]?.answer).toBe("Hello");
    expect(next[0]?.phase).toBe("token");
    const final = mergeChatEvent(next, ev({ phase: "token", token_delta: ", world" }));
    expect(final[0]?.answer).toBe("Hello, world");
  });

  it("accumulates sources across multiple source events in order", () => {
    const turns = [baseTurn()];
    const a = mergeChatEvent(
      turns,
      ev({
        phase: "source",
        sources: [
          {
            source_kind: "raw_memory",
            memory_id: "111",
            source_thread_id: "222",
            snippet: "first",
            score: 0.9,
          },
        ],
      }),
    );
    const b = mergeChatEvent(
      a,
      ev({
        phase: "source",
        sources: [
          {
            source_kind: "thread_summary",
            memory_id: "333",
            source_thread_id: "444",
            snippet: "second",
            score: 0.8,
          },
        ],
      }),
    );
    expect(b[0]?.sources.length).toBe(2);
    expect(b[0]?.sources[0]?.source_kind).toBe("raw_memory");
    expect(b[0]?.sources[1]?.source_kind).toBe("thread_summary");
  });

  it("flips phase to done on the terminal event", () => {
    const turns = [baseTurn({ phase: "token", answer: "answer" })];
    const next = mergeChatEvent(turns, ev({ phase: "done", message: null }));
    expect(next[0]?.phase).toBe("done");
    // answer is preserved (token deltas have already been concatenated)
    expect(next[0]?.answer).toBe("answer");
  });

  it("stores the error message under turn.error", () => {
    const turns = [baseTurn()];
    const next = mergeChatEvent(turns, ev({ phase: "error", message: "boom" }));
    expect(next[0]?.phase).toBe("error");
    expect(next[0]?.error).toBe("boom");
  });

  it("carries the searching message but does not erase prior tokens", () => {
    // A multi-step turn (token → tool call → token again) keeps the
    // tokens collected so far; the searching message is a status
    // breadcrumb only.
    const turns = [baseTurn({ phase: "token", answer: "drafting…" })];
    const next = mergeChatEvent(turns, ev({ phase: "searching", message: "searching memories" }));
    expect(next[0]?.phase).toBe("searching");
    expect(next[0]?.message).toBe("searching memories");
    expect(next[0]?.answer).toBe("drafting…");
  });

  it("ignores events for an unknown job_id (no mutation)", () => {
    const turns = [baseTurn()];
    const next = mergeChatEvent(turns, {
      job_id: "chat-999",
      phase: "token",
      token_delta: "stray",
    });
    expect(next).toBe(turns);
  });

  it("does not regress phase when an out-of-order start arrives", () => {
    // The Rust side emits Start synchronously before the dispatch
    // task starts, but a stale Start from a previous job (same id by
    // accident) should not roll a Token turn back.
    const turns = [baseTurn({ phase: "token", answer: "Hello" })];
    const next = mergeChatEvent(turns, ev({ phase: "start" }));
    expect(next[0]?.phase).toBe("token");
  });

  it("ignores an empty token delta", () => {
    // The Rust extractor filters empty text already; if a stray empty
    // delta does land, don't bump the phase to `token` (would mis-flag
    // the turn as actively generating).
    const turns = [baseTurn({ phase: "searching" })];
    const next = mergeChatEvent(turns, ev({ phase: "token", token_delta: "" }));
    expect(next[0]?.phase).toBe("searching");
    expect(next[0]?.answer).toBe("");
  });
});

describe("buildHistoryMessages — TEST-CHAT-9", () => {
  it("emits role-paired user+assistant messages for each completed turn", () => {
    const turns: ChatTurn[] = [
      baseTurn({
        jobId: "chat-1",
        question: "Q1",
        answer: "A1",
        phase: "done",
      }),
      baseTurn({
        jobId: "chat-2",
        question: "Q2",
        answer: "A2",
        phase: "done",
      }),
    ];
    const msgs = buildHistoryMessages(turns);
    expect(msgs).toEqual([
      { role: "user", content: "Q1" },
      { role: "assistant", content: "A1" },
      { role: "user", content: "Q2" },
      { role: "assistant", content: "A2" },
    ]);
  });

  it("skips turns that errored (no assistant text exists)", () => {
    const turns: ChatTurn[] = [
      baseTurn({
        jobId: "chat-1",
        question: "Q1",
        answer: "",
        phase: "error",
        error: "boom",
      }),
      baseTurn({
        jobId: "chat-2",
        question: "Q2",
        answer: "A2",
        phase: "done",
      }),
    ];
    const msgs = buildHistoryMessages(turns);
    expect(msgs).toEqual([
      { role: "user", content: "Q2" },
      { role: "assistant", content: "A2" },
    ]);
  });

  it("skips turns still in flight (partial token answer)", () => {
    const turns: ChatTurn[] = [
      baseTurn({ jobId: "chat-1", question: "Q1", answer: "A1", phase: "done" }),
      baseTurn({ jobId: "chat-2", question: "Q2", answer: "partial", phase: "token" }),
    ];
    // The in-flight turn's `answer` is the partial reply being
    // streamed; handing it back to the LLM as assistant context would
    // ask the model to continue its own half-finished sentence.
    const msgs = buildHistoryMessages(turns);
    expect(msgs).toEqual([
      { role: "user", content: "Q1" },
      { role: "assistant", content: "A1" },
    ]);
  });

  it("truncates history to the last 5 turns (10 messages)", () => {
    const turns: ChatTurn[] = Array.from({ length: 7 }, (_, i) =>
      baseTurn({
        jobId: `chat-${i}`,
        question: `Q${i}`,
        answer: `A${i}`,
        phase: "done",
      }),
    );
    const msgs = buildHistoryMessages(turns);
    expect(msgs.length).toBe(10);
    // The earliest two turns (Q0/Q1) are dropped; keeps Q2..Q6.
    expect(msgs[0]).toEqual({ role: "user", content: "Q2" });
    expect(msgs[msgs.length - 1]).toEqual({ role: "assistant", content: "A6" });
  });
});

describe("useRagChat.cancel — OPEN-CHAT-2", () => {
  it("dispatches chatCancel with the in-flight turn's jobId", async () => {
    chatCancelMock.mockClear();
    const { result } = renderHook(() => useRagChat());

    await act(async () => {
      await result.current.ask("どうした?");
    });
    expect(result.current.busy).toBe(true);
    const liveJobId = result.current.turns[0]?.jobId;
    expect(liveJobId).toBeDefined();

    await act(async () => {
      await result.current.cancel();
    });
    expect(chatCancelMock).toHaveBeenCalledTimes(1);
    expect(chatCancelMock).toHaveBeenCalledWith(liveJobId);
  });

  it("is a no-op when no turn is in flight (empty history)", async () => {
    chatCancelMock.mockClear();
    const { result } = renderHook(() => useRagChat());
    expect(result.current.busy).toBe(false);
    await act(async () => {
      await result.current.cancel();
    });
    expect(chatCancelMock).not.toHaveBeenCalled();
  });
});
