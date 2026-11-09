import { useCallback, useEffect, useRef, useState } from "react";
import {
  chatAsk,
  chatCancel,
  chatRespondToolApproval,
  getChatToolSettings,
  setChatToolSettings,
} from "@/api";
import type {
  ChatIssueMutationReceipt,
  ChatMessage,
  ChatPhase,
  ChatSource,
  ChatStepUpdate,
  ChatToolApproval,
  RetrievalMode,
  SelectedMemorySnapshot,
} from "@/types/api";
import { useTauriEvent } from "./useTauriEvent";

/** One turn of the in-screen conversation. Lives in React state only —
 *  Nothing is persisted, so a tab switch or
 *  app restart wipes the entire chat. */
export interface ChatTurn {
  /** Correlation key chosen by `ask()` before dispatch and echoed
   *  back on every `chat://step` event for this turn. */
  jobId: string;
  question: string;
  /** Assistant answer accumulated from successive Token deltas. The
   *  empty string is the initial state — render a typing placeholder
   *  until the first Token lands. */
  answer: string;
  sources: ChatSource[];
  phase: ChatPhase;
  /** Human-readable status carried on `searching` / `error` / `done`
   *  events; `token`/`source` leave this null. */
  message: string | null;
  error: string | null;
  cancelled?: boolean;
  selectedMemories?: SelectedMemorySnapshot[];
  selectedContentTruncated?: boolean;
  retrievalMode?: RetrievalMode | null;
  pendingApproval?: ChatToolApproval | null;
  issueMutations?: ChatIssueMutationReceipt[];
}

const MAX_TURNS = 5;
const MAX_HISTORY_MESSAGES = MAX_TURNS * 2;

/** Apply one `chat://step` event to the turn list and return the next
 *  state. Pure so the merge logic is unit-testable without React.
 *
 *  Returns the same reference when nothing changed (unknown job_id and
 *  no-op event) so React's identity-based bail-out skips re-renders. */
export function mergeChatEvent(turns: ChatTurn[], ev: ChatStepUpdate): ChatTurn[] {
  const idx = turns.findIndex((t) => t.jobId === ev.job_id);
  if (idx === -1) {
    // No turn registered yet — `chat_ask`'s caller is expected to
    // append the question turn synchronously before awaiting the
    // promise, so this only happens on a stale event from a previous
    // job that already left the screen. Drop it.
    return turns;
  }
  const current = turns[idx];
  if (!current) return turns;
  const next = applyEvent(current, ev);
  if (next === current) return turns;
  const out = turns.slice();
  out[idx] = next;
  return out;
}

function applyEvent(turn: ChatTurn, ev: ChatStepUpdate): ChatTurn {
  switch (ev.phase) {
    case "start":
      // `ask()` already appended the turn with phase: "start", and we
      // never want a stray late Start to roll the turn back from
      // token/source/searching/done. No-op in every case.
      return turn;
    case "searching":
      return {
        ...turn,
        phase: "searching",
        message: ev.message ?? null,
        pendingApproval: null,
      };
    case "approval-required":
      return {
        ...turn,
        phase: "approval-required",
        message: ev.message ?? null,
        pendingApproval: ev.tool_approval ?? null,
      };
    case "issue-mutation":
      return ev.issue_mutation
        ? { ...turn, issueMutations: [...(turn.issueMutations ?? []), ev.issue_mutation] }
        : turn;
    case "source":
      // Each Source event carries one tool-result batch.
      // Concatenate so multi-tool-call answers preserve every citation
      // observation in arrival order.
      return {
        ...turn,
        phase: "source",
        sources: ev.sources ? [...turn.sources, ...ev.sources] : turn.sources,
        pendingApproval: null,
      };
    case "token":
      // Token deltas are appended verbatim — the Rust side already
      // filters empty deltas (extract_text_delta returns None on "").
      if (!ev.token_delta) return turn;
      return {
        ...turn,
        phase: "token",
        answer: turn.answer + ev.token_delta,
        pendingApproval: null,
      };
    case "done": {
      const msg = ev.message ?? turn.message;
      // Plugin / FinalCollected can deliver `done` twice for the same
      // turn; bail out on the second one so React's setState same-ref
      // shortcut skips a useless re-render of every ChatTurnCard.
      if (turn.phase === "done" && turn.message === msg) return turn;
      return {
        ...turn,
        phase: "done",
        message: msg,
        cancelled: ev.cancelled === true,
        pendingApproval: null,
      };
    }
    case "error":
      return {
        ...turn,
        phase: "error",
        error: ev.message ?? "unknown error",
        pendingApproval: null,
      };
  }
}

/** Build the messages array sent on the next `chat_ask` call from the
 *  current turn list. Only fully-settled turns (phase === "done") feed
 *  back as assistant context: a partial token-streaming turn would
 *  hand the LLM its own half-finished reply, and an errored turn has
 *  no usable assistant text. Truncates to DECIDE-CHAT-6 (5 turns). */
export function buildHistoryMessages(turns: ChatTurn[]): ChatMessage[] {
  const out: ChatMessage[] = [];
  for (const t of turns) {
    if (t.phase !== "done" || t.cancelled || !t.answer) continue;
    out.push({ role: "user", content: t.question });
    out.push({ role: "assistant", content: t.answer });
  }
  return out.slice(-MAX_HISTORY_MESSAGES);
}

export interface UseRagChat {
  turns: ChatTurn[];
  /** Submit a question. The history (last 5 turns) is prepended on the
   *  server-bound `messages` array automatically. */
  ask: (
    question: string,
    selectedMemories?: SelectedMemorySnapshot[],
    retrievalMode?: RetrievalMode,
  ) => Promise<void>;
  /** Wipe the screen-only history. */
  reset: () => void;
  /** Cancel the in-flight turn (OPEN-CHAT-2). Resolves immediately; the
   *  actual terminal-state transition arrives over `chat://step` so the
   *  `busy` flag flips through the same path as a natural Done. No-op when
   *  no turn is currently in flight. */
  cancel: () => Promise<void>;
  /** Current persisted app-wide tool approval policy; true is the safe fallback. */
  requireApproval: boolean;
  /** Persist a new app-wide approval policy before reflecting it in the UI. */
  setRequireApproval: (requireApproval: boolean) => Promise<void>;
  /** Resolve one currently pending tool call, ignoring stale call identifiers. */
  respondToToolApproval: (jobId: string, callId: string, approved: boolean) => Promise<void>;
  /** Mark whether the Chat screen can currently present approval prompts. */
  setApprovalUiActive: (active: boolean) => void;
  /** Reject any approval still pending when Chat leaves the screen. */
  rejectPendingApprovals: () => Promise<void>;
  /** True once a turn is in flight (between dispatch and `done`/`error`).
   *  Drives the input's disabled state. */
  busy: boolean;
}

/** Subscribes to `chat://step` and exposes a stateful conversation log
 *  for the Chat page. Single hook instance per UI tree — the event
 *  stream is broadcast to all listeners by `listen()` underneath, but
 *  the merge state has to live in exactly one place. */
export function useRagChat(): UseRagChat {
  const [turns, setTurns] = useState<ChatTurn[]>([]);
  const [requireApproval, setRequireApprovalState] = useState(true);
  const approvalResponses = useRef(new Set<string>());
  const approvalUiActive = useRef(false);
  // Mirror the latest turns so `ask` (which has an empty deps array to
  // keep its identity stable across renders) can read the current
  // history without going through a setState-with-side-effect hack.
  const turnsRef = useRef(turns);
  useEffect(() => {
    turnsRef.current = turns;
  }, [turns]);

  useEffect(() => {
    let mounted = true;
    void getChatToolSettings()
      .then((settings) => {
        if (mounted) setRequireApprovalState(settings.require_approval);
      })
      .catch(() => {
        // A missing settings response must not silently enable tool execution.
      });
    return () => {
      mounted = false;
    };
  }, []);

  const setApprovalUiActive = useCallback((active: boolean) => {
    approvalUiActive.current = active;
  }, []);

  const respondToToolApproval = useCallback(
    async (jobId: string, callId: string, approved: boolean, allowMissingPending = false) => {
      const turn = turnsRef.current.find((item) => item.jobId === jobId);
      if (!allowMissingPending && turn?.pendingApproval?.call_id !== callId) return;
      const responseKey = JSON.stringify([jobId, callId]);
      if (approvalResponses.current.has(responseKey)) return;

      approvalResponses.current.add(responseKey);
      try {
        await chatRespondToolApproval(jobId, callId, approved);
        setTurns((current) =>
          current.map((item) =>
            item.jobId === jobId && item.pendingApproval?.call_id === callId
              ? { ...item, phase: "searching", message: null, pendingApproval: null }
              : item,
          ),
        );
      } finally {
        approvalResponses.current.delete(responseKey);
      }
    },
    [],
  );

  const updateApprovalSetting = useCallback(
    async (next: boolean) => {
      const settings = await setChatToolSettings({ require_approval: next });
      setRequireApprovalState(settings.require_approval);
      if (!settings.require_approval) {
        const pending = turnsRef.current.flatMap((turn) =>
          turn.pendingApproval ? [{ jobId: turn.jobId, callId: turn.pendingApproval.call_id }] : [],
        );
        await Promise.allSettled(
          pending.map(({ jobId, callId }) => respondToToolApproval(jobId, callId, true)),
        );
      }
    },
    [respondToToolApproval],
  );

  const handleChatStep = useCallback(
    (ev: ChatStepUpdate) => {
      if (ev.phase !== "approval-required") {
        setTurns((prev) => mergeChatEvent(prev, ev));
        return;
      }

      setTurns((prev) => mergeChatEvent(prev, ev));
      if (!ev.tool_approval) {
        if (!approvalUiActive.current) {
          // A malformed pause has no call id to reject, so stop it instead of leaving an invisible wait.
          void chatCancel(ev.job_id).catch((error) => {
            console.error("chat_cancel failed after incomplete approval event", error);
          });
        }
        return;
      }

      if (requireApproval && approvalUiActive.current) return;

      if (!requireApproval) {
        // OFF means every tool call runs without a per-call confirmation, even if a pause event arrives.
        setTurns((prev) =>
          mergeChatEvent(prev, ev).map((turn) =>
            turn.jobId === ev.job_id
              ? { ...turn, phase: "searching", message: null, pendingApproval: null }
              : turn,
          ),
        );
        void respondToToolApproval(ev.job_id, ev.tool_approval.call_id, true, true).catch(
          (error) => {
            console.error("chat tool auto-approval failed", error);
            if (approvalUiActive.current) {
              setTurns((prev) =>
                prev.map((turn) =>
                  turn.jobId === ev.job_id
                    ? { ...turn, phase: "approval-required", pendingApproval: ev.tool_approval }
                    : turn,
                ),
              );
              return;
            }
            void chatCancel(ev.job_id).catch((cancelError) => {
              console.error("chat_cancel failed after automatic approval", cancelError);
            });
          },
        );
        return;
      }

      void respondToToolApproval(ev.job_id, ev.tool_approval.call_id, false, true).catch(
        (error) => {
          console.error("hidden chat tool approval rejection failed", error);
          void chatCancel(ev.job_id).catch((cancelError) => {
            console.error("chat_cancel failed after hidden approval", cancelError);
          });
        },
      );
    },
    [requireApproval, respondToToolApproval],
  );

  useTauriEvent<ChatStepUpdate>("chat://step", handleChatStep);

  const rejectPendingApprovals = useCallback(async () => {
    const pending = turnsRef.current.flatMap((turn) =>
      turn.pendingApproval ? [{ jobId: turn.jobId, callId: turn.pendingApproval.call_id }] : [],
    );
    const outcomes = await Promise.allSettled(
      pending.map(({ jobId, callId }) => respondToToolApproval(jobId, callId, false)),
    );
    for (const outcome of outcomes) {
      if (outcome.status === "rejected") {
        console.error("chat tool approval rejection failed", outcome.reason);
      }
    }
  }, [respondToToolApproval]);

  const ask = useCallback(
    async (
      question: string,
      selectedMemories: SelectedMemorySnapshot[] = [],
      retrievalMode?: RetrievalMode,
    ) => {
      const trimmed = question.trim();
      if (!trimmed) return;
      const history = buildHistoryMessages(turnsRef.current);

      // Register the turn BEFORE invoking `chat_ask`. The Rust side
      // emits `Start` synchronously during the command body, and the
      // detached stream task can land `Token`/`Error` before the
      // command's response has even reached us. Pre-registering with a
      // client-chosen `jobId` means those events never hit an unknown
      // turn (mergeChatEvent would otherwise drop them).
      const jobId = `chat-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
      setTurns((prev) => [
        ...prev,
        {
          jobId,
          question: trimmed,
          answer: "",
          sources: [],
          phase: "start",
          message: null,
          error: null,
          selectedMemories: structuredClone(selectedMemories),
          retrievalMode: retrievalMode ?? (selectedMemories.length > 0 ? "source_details" : null),
        },
      ]);

      try {
        const response = await chatAsk({
          messages: [...history, { role: "user", content: trimmed }],
          job_id: jobId,
          selected_memories: selectedMemories,
          retrieval_mode: retrievalMode,
        });
        if (response.selected_content_truncated) {
          setTurns((prev) =>
            prev.map((turn) =>
              turn.jobId === jobId ? { ...turn, selectedContentTruncated: true } : turn,
            ),
          );
        }
      } catch (e) {
        // `chat_ask` rejected before the stream could start (sidecar
        // down, validation error, etc.). Surface it on the turn we
        // already registered so the user sees what went wrong.
        const msg = e instanceof Error ? e.message : String(e);
        setTurns((prev) =>
          mergeChatEvent(prev, {
            job_id: jobId,
            phase: "error",
            message: msg,
          }),
        );
        throw e;
      }
    },
    [],
  );

  const reset = useCallback(() => setTurns([]), []);

  // Cancel only fires on a still-running turn — the Rust side returns
  // `false` for unknown/finished jobIds, but we don't even bother calling
  // when the latest turn already settled.
  const cancel = useCallback(async () => {
    const list = turnsRef.current;
    const last = list[list.length - 1];
    if (!last || last.phase === "done" || last.phase === "error") return;
    if (last.pendingApproval) {
      try {
        await respondToToolApproval(last.jobId, last.pendingApproval.call_id, false);
      } catch (e) {
        // Still send cancellation so a failed approval response cannot keep the turn running.
        console.error("chat tool approval rejection failed", e);
      }
    }
    try {
      await chatCancel(last.jobId);
    } catch (e) {
      console.error("chat_cancel failed", e);
    }
  }, [respondToToolApproval]);

  // A turn is in flight while its phase is not the terminal done/error.
  // Empty `turns` ⇒ idle. We check only the last turn because the chat
  // flow is strictly sequential — the user can't submit until the
  // previous turn settles (see `Chat.tsx` ask gating).
  const last = turns[turns.length - 1];
  const busy = last != null && last.phase !== "done" && last.phase !== "error";

  return {
    turns,
    ask,
    reset,
    cancel,
    busy,
    requireApproval,
    setRequireApproval: updateApprovalSetting,
    respondToToolApproval,
    setApprovalUiActive,
    rejectPendingApprovals,
  };
}
