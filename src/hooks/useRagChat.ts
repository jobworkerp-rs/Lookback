import { useCallback, useEffect, useRef, useState } from "react";
import { chatAsk, chatCancel } from "@/api";
import type { ChatMessage, ChatPhase, ChatSource, ChatStepUpdate } from "@/types/api";
import { useTauriEvent } from "./useTauriEvent";

/** One turn of the in-screen conversation. Lives in React state only —
 *  per FR-CHAT-6 / NG-CHAT-1 nothing is persisted, so a tab switch or
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
      };
    case "source":
      // Each Source event carries one tool-result batch (FR-CHAT-4a).
      // Concatenate so multi-tool-call answers preserve every citation
      // observation in arrival order.
      return {
        ...turn,
        phase: "source",
        sources: ev.sources ? [...turn.sources, ...ev.sources] : turn.sources,
      };
    case "token":
      // Token deltas are appended verbatim — the Rust side already
      // filters empty deltas (extract_text_delta returns None on "").
      if (!ev.token_delta) return turn;
      return {
        ...turn,
        phase: "token",
        answer: turn.answer + ev.token_delta,
      };
    case "done": {
      const msg = ev.message ?? turn.message;
      // Plugin / FinalCollected can deliver `done` twice for the same
      // turn; bail out on the second one so React's setState same-ref
      // shortcut skips a useless re-render of every ChatTurnCard.
      if (turn.phase === "done" && turn.message === msg) return turn;
      return { ...turn, phase: "done", message: msg };
    }
    case "error":
      return {
        ...turn,
        phase: "error",
        error: ev.message ?? "unknown error",
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
    if (t.phase !== "done" || !t.answer) continue;
    out.push({ role: "user", content: t.question });
    out.push({ role: "assistant", content: t.answer });
  }
  return out.slice(-MAX_HISTORY_MESSAGES);
}

export interface UseRagChat {
  turns: ChatTurn[];
  /** Submit a question. The history (last 5 turns) is prepended on the
   *  server-bound `messages` array automatically. */
  ask: (question: string) => Promise<void>;
  /** Wipe the screen-only history (FR-CHAT-6 / NG-CHAT-1). */
  reset: () => void;
  /** Cancel the in-flight turn (OPEN-CHAT-2). Resolves immediately; the
   *  actual terminal-state transition arrives over `chat://step` so the
   *  `busy` flag flips through the same path as a natural Done. No-op when
   *  no turn is currently in flight. */
  cancel: () => Promise<void>;
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
  // Mirror the latest turns so `ask` (which has an empty deps array to
  // keep its identity stable across renders) can read the current
  // history without going through a setState-with-side-effect hack.
  const turnsRef = useRef(turns);
  useEffect(() => {
    turnsRef.current = turns;
  }, [turns]);

  useTauriEvent<ChatStepUpdate>("chat://step", (ev) => {
    setTurns((prev) => mergeChatEvent(prev, ev));
  });

  const ask = useCallback(async (question: string) => {
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
      },
    ]);

    try {
      await chatAsk({
        messages: [...history, { role: "user", content: trimmed }],
        job_id: jobId,
      });
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
  }, []);

  const reset = useCallback(() => setTurns([]), []);

  // Cancel only fires on a still-running turn — the Rust side returns
  // `false` for unknown/finished jobIds, but we don't even bother calling
  // when the latest turn already settled.
  const cancel = useCallback(async () => {
    const list = turnsRef.current;
    const last = list[list.length - 1];
    if (!last || last.phase === "done" || last.phase === "error") return;
    try {
      await chatCancel(last.jobId);
    } catch (e) {
      console.error("chat_cancel failed", e);
    }
  }, []);

  // A turn is in flight while its phase is not the terminal done/error.
  // Empty `turns` ⇒ idle. We check only the last turn because the chat
  // flow is strictly sequential — the user can't submit until the
  // previous turn settles (see `Chat.tsx` ask gating).
  const last = turns[turns.length - 1];
  const busy = last != null && last.phase !== "done" && last.phase !== "error";

  return { turns, ask, reset, cancel, busy };
}
