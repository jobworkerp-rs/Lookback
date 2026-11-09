import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ChatStepUpdate } from "@/types/api";

const chatAskMock = vi.fn(async () => ({ job_id: "ignored" }));
const chatCancelMock = vi.fn(async (_jobId: string): Promise<void> => undefined);
const chatRespondToolApprovalMock = vi.fn(
  async (_jobId: string, _callId: string, _approved: boolean): Promise<void> => undefined,
);
const getChatToolSettingsMock = vi.fn(async () => ({ require_approval: true }));
const setChatToolSettingsMock = vi.fn(async (req: { require_approval: boolean }) => req);
const chatStepListeners = vi.hoisted(() => [] as Array<(payload: unknown) => void>);
vi.mock("@/api", () => ({
  chatAsk: () => chatAskMock(),
  chatCancel: (jobId: string) => chatCancelMock(jobId),
  chatRespondToolApproval: (jobId: string, callId: string, approved: boolean) =>
    chatRespondToolApprovalMock(jobId, callId, approved),
  getChatToolSettings: () => getChatToolSettingsMock(),
  setChatToolSettings: (req: { require_approval: boolean }) => setChatToolSettingsMock(req),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((_event: string, handler: (event: { payload: unknown }) => void) => {
    const listener = (payload: unknown) => handler({ payload });
    chatStepListeners.push(listener);
    return Promise.resolve(() => {
      const index = chatStepListeners.indexOf(listener);
      if (index >= 0) chatStepListeners.splice(index, 1);
    });
  }),
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

function emitChatEvent(event: ChatStepUpdate) {
  for (const listener of chatStepListeners) listener(event);
}

beforeEach(() => {
  chatAskMock.mockClear();
  chatCancelMock.mockClear();
  chatRespondToolApprovalMock.mockClear();
  getChatToolSettingsMock.mockReset().mockResolvedValue({ require_approval: true });
  setChatToolSettingsMock.mockReset().mockImplementation(async (req) => req);
  chatStepListeners.length = 0;
});

describe("mergeChatEvent — TEST-CHAT-8", () => {
  it("retains confirmed Issue receipts when the LLM answer omits them", () => {
    const receipt = {
      tool_name: "lookback_issue_create",
      issue_id: "issue-42",
      operation_id: "op-42",
      operation_state: "COMMITTED",
      audit_reason: "requested by the user",
    };
    const afterMutation = mergeChatEvent(
      [baseTurn()],
      ev({ phase: "issue-mutation", issue_mutation: receipt }),
    );
    const done = mergeChatEvent(afterMutation, ev({ phase: "done" }));
    expect(done[0]?.issueMutations).toEqual([receipt]);
  });
  it("appends a token delta to the matching turn's answer", () => {
    const turns = [baseTurn()];
    const next = mergeChatEvent(turns, ev({ phase: "token", token_delta: "Hello" }));
    expect(next[0]?.answer).toBe("Hello");
    expect(next[0]?.phase).toBe("token");
    const final = mergeChatEvent(next, ev({ phase: "token", token_delta: ", world" }));
    expect(final[0]?.answer).toBe("Hello, world");
  });

  it("marks a done event from cancellation so partial answers are not reusable", () => {
    const next = mergeChatEvent(
      [baseTurn({ answer: "partial", phase: "token" })],
      ev({ phase: "done", message: "cancelled", cancelled: true }),
    );
    expect(next[0]).toMatchObject({ phase: "done", answer: "partial", cancelled: true });
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
            source_kind: "thread_group_summary",
            memory_id: "333",
            group_id: "444",
            snippet: "second",
            score: 0.8,
          },
        ],
      }),
    );
    expect(b[0]?.sources.length).toBe(2);
    expect(b[0]?.sources[0]?.source_kind).toBe("raw_memory");
    expect(b[0]?.sources[1]?.source_kind).toBe("thread_group_summary");
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

  it("keeps the tool name and exact arguments visible while approval is pending", () => {
    const approval = {
      call_id: "call-4",
      tool_name: "lookback_issue_create",
      arguments_json: '{"title":"Fix the flaky test"}',
    };

    const pending = mergeChatEvent(
      [baseTurn({ answer: "I can create that issue.", phase: "token" })],
      ev({ phase: "approval-required", tool_approval: approval }),
    );
    expect(pending[0]).toMatchObject({ phase: "approval-required", pendingApproval: approval });

    const settled = mergeChatEvent(pending, ev({ phase: "done", message: "finished" }));
    expect(settled[0]?.pendingApproval).toBeNull();
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

  it("rejects an outstanding tool approval before cancelling its turn", async () => {
    const { result } = renderHook(() => useRagChat());
    await act(async () => {
      await result.current.ask("create an issue");
    });
    const jobId = result.current.turns[0]?.jobId;
    expect(jobId).toBeDefined();
    act(() => result.current.setApprovalUiActive(true));

    await act(async () => {
      emitChatEvent({
        job_id: jobId ?? "",
        phase: "approval-required",
        tool_approval: {
          call_id: "call-cancel",
          tool_name: "lookback_issue_create",
          arguments_json: '{"title":"New issue"}',
        },
      });
    });
    await act(async () => {
      await result.current.cancel();
    });

    expect(chatRespondToolApprovalMock).toHaveBeenCalledWith(jobId, "call-cancel", false);
    expect(chatCancelMock).toHaveBeenCalledWith(jobId);
    expect(chatRespondToolApprovalMock.mock.invocationCallOrder[0]).toBeLessThan(
      chatCancelMock.mock.invocationCallOrder[0] ?? Number.POSITIVE_INFINITY,
    );
  });

  it("rejects tool calls that arrive while the Chat screen is not mounted", async () => {
    const { result } = renderHook(() => useRagChat());
    await act(async () => {
      await result.current.ask("create an issue");
    });
    const jobId = result.current.turns[0]?.jobId;

    await act(async () => {
      emitChatEvent({
        job_id: jobId ?? "",
        phase: "approval-required",
        tool_approval: {
          call_id: "call-hidden",
          tool_name: "lookback_issue_create",
          arguments_json: '{"title":"New issue"}',
        },
      });
      await Promise.resolve();
    });

    expect(chatRespondToolApprovalMock).toHaveBeenCalledWith(jobId, "call-hidden", false);
    expect(result.current.turns[0]?.pendingApproval).toBeNull();
  });

  it("automatically approves tool calls when the persisted policy is off", async () => {
    const { result } = renderHook(() => useRagChat());
    await act(async () => {
      await result.current.setRequireApproval(false);
      await result.current.ask("create an issue");
    });
    const jobId = result.current.turns[0]?.jobId;
    act(() => result.current.setApprovalUiActive(true));

    await act(async () => {
      emitChatEvent({
        job_id: jobId ?? "",
        phase: "approval-required",
        tool_approval: {
          call_id: "call-auto",
          tool_name: "lookback_issue_create",
          arguments_json: '{"title":"New issue"}',
        },
      });
      await Promise.resolve();
    });

    expect(chatRespondToolApprovalMock).toHaveBeenCalledWith(jobId, "call-auto", true);
    expect(result.current.turns[0]).toMatchObject({ phase: "searching", pendingApproval: null });
  });

  it("releases an already pending approval when the user switches the policy off", async () => {
    const { result } = renderHook(() => useRagChat());
    await act(async () => {
      await result.current.ask("create an issue");
    });
    const jobId = result.current.turns[0]?.jobId ?? "";
    act(() => result.current.setApprovalUiActive(true));
    await act(async () => {
      emitChatEvent({
        job_id: jobId,
        phase: "approval-required",
        tool_approval: {
          call_id: "pending-1",
          tool_name: "lookback_issue_create",
          arguments_json: "{}",
        },
      });
      await Promise.resolve();
    });
    expect(result.current.turns[0]?.pendingApproval?.call_id).toBe("pending-1");
    await act(async () => {
      await result.current.setRequireApproval(false);
    });
    expect(chatRespondToolApprovalMock).toHaveBeenCalledWith(jobId, "pending-1", true);
    expect(result.current.turns[0]?.pendingApproval).toBeNull();
  });

  it("loads persisted approval mode but keeps approval enabled if reading settings fails", async () => {
    getChatToolSettingsMock.mockResolvedValueOnce({ require_approval: false });
    const { result, rerender } = renderHook(() => useRagChat());
    await act(async () => {
      await Promise.resolve();
    });
    expect(result.current.requireApproval).toBe(false);

    rerender();
    expect(result.current.requireApproval).toBe(false);

    getChatToolSettingsMock.mockRejectedValueOnce(new Error("settings unavailable"));
    const fallback = renderHook(() => useRagChat());
    await act(async () => {
      await Promise.resolve();
    });
    expect(fallback.result.current.requireApproval).toBe(true);
  });
});
