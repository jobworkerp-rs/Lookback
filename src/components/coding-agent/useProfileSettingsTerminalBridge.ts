import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  type ProfileTerminalSession,
  profileTerminalOutput,
  profileTerminalSendInput,
  profileTerminalSession,
} from "@/api/profileTerminal";
import type { AgentTerminalBridge, TerminalFeedStatus } from "./types";

interface Snapshot {
  session: ProfileTerminalSession | null;
  feedStatus: TerminalFeedStatus;
  error: string | null;
}
export function useProfileSettingsTerminalBridge(sessionId: string | null) {
  const [snapshot, setSnapshot] = useState<Snapshot>({
    session: null,
    feedStatus: "unavailable",
    error: null,
  });
  const current = useRef(snapshot);
  current.current = snapshot;
  const selected = useRef(sessionId);
  selected.current = sessionId;
  const listeners = useRef(new Set<{ id: string; receive: (bytes: Uint8Array) => void }>());
  const pending = useRef<Uint8Array[]>([]);
  const cursor = useRef("0");
  const cursorSession = useRef<string | null>(null);
  const sawEnd = useRef(false);
  const stopped = useRef(true);
  const generation = useRef(0);
  const [attachment, setAttachment] = useState(0);

  const reattach = useCallback(() => setAttachment((value) => value + 1), []);
  // biome-ignore lint/correctness/useExhaustiveDependencies: Explicit reattachment replaces the reader after a transport failure.
  useEffect(() => {
    const activeGeneration = ++generation.current;
    let detached = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    stopped.current = !sessionId;
    if (cursorSession.current !== sessionId) {
      pending.current = [];
      cursor.current = "0";
      cursorSession.current = sessionId;
      sawEnd.current = false;
    }
    const initial: Snapshot = { session: null, feedStatus: "unavailable", error: null };
    current.current = initial;
    setSnapshot(initial);
    if (!sessionId) return;
    const isCurrent = () =>
      !detached && generation.current === activeGeneration && selected.current === sessionId;
    const poll = async () => {
      try {
        const page = await profileTerminalOutput(sessionId, cursor.current);
        const session = await profileTerminalSession(sessionId);
        if (!isCurrent() || stopped.current) return;
        for (const record of page.events) {
          if (record.event.kind !== "output") continue;
          const bytes = new Uint8Array(record.event.data);
          const receivers = [...listeners.current].filter((listener) => listener.id === sessionId);
          if (receivers.length) {
            for (const listener of receivers) listener.receive(bytes);
          } else {
            pending.current.push(bytes);
            if (
              pending.current.length > 512 ||
              pending.current.reduce((size, chunk) => size + chunk.byteLength, 0) > 1024 * 1024
            )
              throw new Error("Terminal display buffer is unavailable");
          }
        }
        cursor.current = page.nextSequence;
        sawEnd.current ||= page.events.some((record) => record.event.kind === "end");
        const next: Snapshot = {
          session,
          feedStatus: session.state === "unknown" ? "unknown" : "active",
          error: null,
        };
        current.current = next;
        setSnapshot(next);
        if (!sawEnd.current || page.events.length === 16)
          timer = setTimeout(
            poll,
            page.events.length === 16 ? 0 : session.state === "unknown" ? 1000 : 200,
          );
      } catch {
        if (!isCurrent()) return;
        stopped.current = true;
        const next: Snapshot = {
          ...current.current,
          feedStatus: "disconnected",
          error: "profile_terminal_feed_unavailable",
        };
        current.current = next;
        setSnapshot(next);
      }
    };
    void poll();
    return () => {
      detached = true;
      stopped.current = true;
      if (timer) clearTimeout(timer);
    };
  }, [sessionId, attachment]);

  const bridge = useMemo<AgentTerminalBridge>(
    () => ({
      subscribeOutput: (id, receive) => {
        if (id !== selected.current) return () => {};
        const listener = { id, receive };
        listeners.current.add(listener);
        const queued = pending.current;
        pending.current = [];
        for (const bytes of queued) receive(bytes);
        return () => {
          listeners.current.delete(listener);
        };
      },
      sendInput: async (id, bytes) => {
        const session = current.current.session;
        if (
          id !== selected.current ||
          session?.sessionId !== id ||
          session.state !== "running" ||
          session.inputClosed ||
          current.current.feedStatus !== "active" ||
          stopped.current
        )
          throw new Error("Profile terminal input is unavailable");
        const activeGeneration = generation.current;
        try {
          await profileTerminalSendInput(id, bytes);
        } catch (error) {
          if (id === selected.current && generation.current === activeGeneration) {
            stopped.current = true;
            const next: Snapshot = {
              ...current.current,
              feedStatus: "disconnected",
              error: "profile_terminal_input_unknown",
            };
            current.current = next;
            setSnapshot(next);
          }
          throw error;
        }
      },
    }),
    [],
  );
  return { ...snapshot, bridge, reattach };
}
