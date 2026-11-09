import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  type NativeTerminalOutputEvent,
  type NativeTerminalOutputRecord,
  type NativeTerminalSession,
  nativeTerminalAttach,
  nativeTerminalDetach,
  nativeTerminalOutput,
  nativeTerminalSendInput,
  parseNativeTerminalOutputEvent,
} from "@/api/nativeTerminal";
import type { NativeTerminalBridgeState } from "./nativeTerminalTypes";
import {
  hasNativeTerminalInputBlocker,
  mergeNativeTerminalSessionFacts,
} from "./nativeTerminalTypes";
import type { AgentTerminalBridge, TerminalFeedStatus } from "./types";

const MAX_TERMINAL_INPUT_BYTES = 64 * 1024;
const MAX_BUFFERED_EVENTS = 512;
const MAX_BUFFERED_OUTPUT_BYTES = 1024 * 1024;

type Snapshot = Omit<NativeTerminalBridgeState, "reattach">;

const MAX_REMEMBERED_DETACHMENTS = 128;

/** @internal Detach attempts are coalesced while pending; only a bounded success FIFO is retained. */
export function createDetachSubscriptionManager(
  detach: typeof nativeTerminalDetach,
  maxRemembered = MAX_REMEMBERED_DETACHMENTS,
): (sessionId: string, subscriptionId: string) => Promise<void> {
  const inFlightDetachments = new Map<string, Promise<void>>();
  const completedDetachments = new Set<string>();

  return (sessionId, subscriptionId) => {
    const key = JSON.stringify([sessionId, subscriptionId]);
    if (completedDetachments.has(key)) return Promise.resolve();
    const existing = inFlightDetachments.get(key);
    if (existing) return existing;
    const request = detach({ sessionId, subscriptionId });
    let trackedRequest: Promise<void>;
    trackedRequest = request.then(
      () => {
        if (inFlightDetachments.get(key) !== trackedRequest) return;
        inFlightDetachments.delete(key);
        completedDetachments.add(key);
        if (completedDetachments.size > maxRemembered) {
          const oldest = completedDetachments.values().next().value;
          if (oldest !== undefined) completedDetachments.delete(oldest);
        }
      },
      (error: unknown) => {
        if (inFlightDetachments.get(key) === trackedRequest) inFlightDetachments.delete(key);
        throw error;
      },
    );
    inFlightDetachments.set(key, trackedRequest);
    return trackedRequest;
  };
}

const detachSubscription = createDetachSubscriptionManager(nativeTerminalDetach);

interface PendingOutput {
  chunks: { sequence: bigint; bytes: Uint8Array }[];
  byteLength: number;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Attaches the renderer to a durable native session without changing its lifetime. */
export function useNativeTerminalBridge(
  sessionId: string | null,
  listedSession: NativeTerminalSession | null = null,
): NativeTerminalBridgeState {
  const [snapshot, setSnapshot] = useState<Snapshot>({
    session: null,
    ownsSession: false,
    feedStatus: "unavailable",
    error: null,
    bridge: { subscribeOutput: () => () => {}, sendInput: async () => {} },
  });
  const snapshotRef = useRef(snapshot);
  snapshotRef.current = snapshot;
  const selectedSessionIdRef = useRef(sessionId);
  selectedSessionIdRef.current = sessionId;
  const listedSessionRef = useRef(listedSession);
  listedSessionRef.current = listedSession;
  const [attachRevision, setAttachRevision] = useState(0);
  const attachGenerationRef = useRef(0);
  const inputPermissionRef = useRef({
    sessionId: null as string | null,
    generation: 0,
    blocked: true,
  });
  const lastSequences = useRef(new Map<string, bigint>());
  const detachErrors = useRef(new Map<string, string>());
  const outputListeners = useRef(new Map<string, Set<(bytes: Uint8Array) => void>>());
  const pendingOutput = useRef(new Map<string, PendingOutput>());
  const rendererBaselines = useRef(new Map<string, bigint>());
  const rendererReplayCursors = useRef(new Map<string, bigint>());
  const rendererReplayGenerations = useRef(new Map<string, number>());

  const reattach = useCallback(() => {
    setAttachRevision((revision) => revision + 1);
  }, []);

  const subscribeOutput = useCallback<AgentTerminalBridge["subscribeOutput"]>(
    (targetSessionId, listener) => {
      const listeners = outputListeners.current.get(targetSessionId) ?? new Set();
      const replayCursor = rendererReplayCursors.current.get(targetSessionId);
      const pending = pendingOutput.current.get(targetSessionId);
      if (listeners.size === 0) {
        const firstQueuedSequence = pending?.chunks[0]?.sequence;
        const rendererBaseline =
          replayCursor ??
          (firstQueuedSequence !== undefined
            ? firstQueuedSequence - 1n
            : (lastSequences.current.get(targetSessionId) ?? 0n));
        rendererBaselines.current.set(targetSessionId, rendererBaseline);
      }
      listeners.add(listener);
      outputListeners.current.set(targetSessionId, listeners);
      if (replayCursor !== undefined) {
        // Native output is durable. Replay from before the previous terminal
        // generation instead of trusting a write queue being disposed later.
        pendingOutput.current.delete(targetSessionId);
        lastSequences.current.set(targetSessionId, replayCursor);
        reattach();
      } else if (pending) {
        let delivered = true;
        let deliveredChunks = 0;
        let deliveredBytes = 0;
        for (const chunk of pending.chunks) {
          try {
            listener(chunk.bytes);
            deliveredChunks += 1;
            deliveredBytes += chunk.bytes.byteLength;
          } catch {
            delivered = false;
            break;
          }
        }
        if (pendingOutput.current.get(targetSessionId) === pending && deliveredChunks > 0) {
          pending.chunks.splice(0, deliveredChunks);
          pending.byteLength -= deliveredBytes;
        }
        if (
          delivered &&
          pending.chunks.length === 0 &&
          pendingOutput.current.get(targetSessionId) === pending
        ) {
          pendingOutput.current.delete(targetSessionId);
        }
      }
      return () => {
        listeners.delete(listener);
        if (listeners.size === 0) {
          outputListeners.current.delete(targetSessionId);
          const baseline = rendererBaselines.current.get(targetSessionId);
          if (baseline !== undefined && selectedSessionIdRef.current === targetSessionId) {
            rendererReplayCursors.current.set(targetSessionId, baseline);
            lastSequences.current.set(targetSessionId, baseline);
          }
          rendererBaselines.current.delete(targetSessionId);
        }
      };
    },
    [reattach],
  );

  const sendInput = useCallback<AgentTerminalBridge["sendInput"]>(
    async (targetSessionId, bytes) => {
      const current = snapshotRef.current;
      const session = current.session;
      const listed = listedSessionRef.current;
      const listedForSession = listed?.sessionId === targetSessionId ? listed : null;
      const mergedSession = mergeNativeTerminalSessionFacts(listedForSession, session);
      const permission = inputPermissionRef.current;
      if (bytes.byteLength > MAX_TERMINAL_INPUT_BYTES) {
        throw new Error("native terminal input exceeds the limit");
      }
      if (
        selectedSessionIdRef.current !== targetSessionId ||
        permission.sessionId !== targetSessionId ||
        permission.blocked ||
        session?.sessionId !== targetSessionId ||
        session.state !== "running" ||
        hasNativeTerminalInputBlocker(session) ||
        (listedForSession !== null && hasNativeTerminalInputBlocker(listedForSession)) ||
        mergedSession.protocolError !== null ||
        !current.ownsSession ||
        current.feedStatus !== "active"
      ) {
        throw new Error("native terminal input delivery was not confirmed");
      }
      const generation = permission.generation;
      const isCurrentGeneration = () =>
        selectedSessionIdRef.current === targetSessionId &&
        inputPermissionRef.current.sessionId === targetSessionId &&
        inputPermissionRef.current.generation === generation;
      const blockInput = (feedStatus: TerminalFeedStatus, message: string) => {
        if (!isCurrentGeneration()) return;
        inputPermissionRef.current = { ...inputPermissionRef.current, blocked: true };
        const latest = snapshotRef.current;
        if (latest.session?.sessionId !== targetSessionId) return;
        const next: Snapshot = { ...latest, feedStatus, error: message };
        snapshotRef.current = next;
        setSnapshot(next);
      };

      let delivery: Awaited<ReturnType<typeof nativeTerminalSendInput>>;
      try {
        delivery = await nativeTerminalSendInput(targetSessionId, bytes);
      } catch (error) {
        blockInput("unknown", errorMessage(error));
        throw error;
      }
      if (!isCurrentGeneration()) {
        throw new Error("native terminal input belongs to an old attachment");
      }
      if (delivery !== "accepted") {
        const feedStatus = delivery === "unavailable" ? "unavailable" : "unknown";
        blockInput(feedStatus, "Native terminal input delivery was not confirmed.");
        throw new Error("native terminal input delivery was not confirmed");
      }
    },
    [],
  );

  const bridge = useMemo<AgentTerminalBridge>(
    () => ({ subscribeOutput, sendInput }),
    [sendInput, subscribeOutput],
  );

  // Reattachment deliberately restarts this listener effect without changing the session ID.
  // biome-ignore lint/correctness/useExhaustiveDependencies: attachRevision is an explicit retry trigger.
  useEffect(() => {
    let cancelled = false;
    const generation = ++attachGenerationRef.current;
    if (sessionId && rendererReplayCursors.current.has(sessionId)) {
      rendererReplayGenerations.current.set(sessionId, generation);
    }
    inputPermissionRef.current = { sessionId, generation, blocked: true };
    let releaseListener: (() => void) | null = null;
    let subscriptionId: string | null = null;
    let attached = false;
    let disconnected = false;
    let draining = false;
    let bufferedEventBytes = 0;
    const earlyEvents: NativeTerminalOutputEvent[] = [];
    const pendingRecords = new Map<bigint, NativeTerminalOutputRecord>();
    const queuedEvents = new Set<string>();
    let lastSequence = sessionId ? (lastSequences.current.get(sessionId) ?? 0n) : 0n;
    let targetSequence = 1n;
    const waitingForRendererReplay = () =>
      sessionId !== null &&
      rendererReplayCursors.current.has(sessionId) &&
      rendererReplayGenerations.current.get(sessionId) !== generation;

    const setCurrent = (next: Omit<Snapshot, "bridge">) => {
      if (
        cancelled ||
        attachGenerationRef.current !== generation ||
        selectedSessionIdRef.current !== sessionId
      ) {
        return;
      }
      const value: Snapshot = { ...next, bridge };
      snapshotRef.current = value;
      setSnapshot(value);
    };

    const blockCurrentInput = () => {
      if (
        inputPermissionRef.current.sessionId === sessionId &&
        inputPermissionRef.current.generation === generation
      ) {
        inputPermissionRef.current = { ...inputPermissionRef.current, blocked: true };
      }
    };

    const reportDetachFailure = (error: unknown) => {
      if (!sessionId) return;
      const message = `Native terminal subscription cleanup failed: ${errorMessage(error)}`;
      detachErrors.current.set(sessionId, message);
      if (
        selectedSessionIdRef.current !== sessionId ||
        snapshotRef.current.session?.sessionId !== sessionId
      ) {
        return;
      }
      const current = snapshotRef.current;
      const next: Snapshot = { ...current, error: message };
      snapshotRef.current = next;
      setSnapshot(next);
    };

    const disconnect = (message: string) => {
      disconnected = true;
      blockCurrentInput();
      const current = snapshotRef.current;
      setCurrent({
        session: current.session,
        ownsSession: current.ownsSession,
        feedStatus: "disconnected",
        error: message,
      });
    };

    const deliverOutput = (record: NativeTerminalOutputRecord): boolean => {
      if (record.event.kind !== "output") return true;
      const bytes = Uint8Array.from(record.event.data);
      const listeners = outputListeners.current.get(sessionId ?? "");
      if (listeners && listeners.size > 0) {
        for (const listener of listeners) {
          try {
            listener(bytes);
          } catch {
            disconnect("Native terminal output renderer failed.");
            return false;
          }
        }
        return true;
      }
      const pending = pendingOutput.current.get(sessionId ?? "") ?? {
        chunks: [],
        byteLength: 0,
      };
      if (pending.byteLength + bytes.byteLength > MAX_BUFFERED_OUTPUT_BYTES) {
        disconnect("Native terminal output arrived before the renderer could attach.");
        return false;
      }
      pending.chunks.push({ sequence: BigInt(record.sequence), bytes });
      pending.byteLength += bytes.byteLength;
      pendingOutput.current.set(sessionId ?? "", pending);
      return true;
    };

    const applyRecord = (record: NativeTerminalOutputRecord): boolean => {
      if (record.event.kind === "output") return deliverOutput(record);
      const current = snapshotRef.current;
      const session = current.session;
      if (!session || session.sessionId !== record.sessionId) return true;
      if (record.event.kind === "exit") {
        blockCurrentInput();
        setCurrent({
          session: { ...session, exitCode: record.event.exitCode },
          ownsSession: current.ownsSession,
          feedStatus: "unavailable",
          error: current.error,
        });
      } else {
        blockCurrentInput();
        setCurrent({
          session: { ...session, endReceived: true, streamEof: true },
          ownsSession: current.ownsSession,
          feedStatus: "disconnected",
          error: current.error,
        });
      }
      return true;
    };

    const commitRecord = (record: NativeTerminalOutputRecord): boolean => {
      if (!applyRecord(record)) return false;
      lastSequence = BigInt(record.sequence);
      if (sessionId) lastSequences.current.set(sessionId, lastSequence);
      return true;
    };

    const acceptRecord = (record: NativeTerminalOutputRecord) => {
      if (waitingForRendererReplay()) return;
      if (record.sessionId !== sessionId) return;
      const sequence = BigInt(record.sequence);
      if (sequence <= lastSequence || queuedEvents.has(record.sequence)) return;
      targetSequence = sequence + 1n > targetSequence ? sequence + 1n : targetSequence;
      if (sequence !== lastSequence + 1n) {
        pendingRecords.set(sequence, record);
        queuedEvents.add(record.sequence);
        bufferedEventBytes += record.event.kind === "output" ? record.event.data.length : 0;
        if (
          pendingRecords.size > MAX_BUFFERED_EVENTS ||
          bufferedEventBytes > MAX_BUFFERED_OUTPUT_BYTES
        ) {
          disconnect("Native terminal output backlog exceeded the safe replay limit.");
          return;
        }
      } else if (!commitRecord(record)) {
        return;
      }

      while (!disconnected && !cancelled) {
        const expected = lastSequence + 1n;
        const pending = pendingRecords.get(expected);
        if (!pending) break;
        pendingRecords.delete(expected);
        queuedEvents.delete(pending.sequence);
        bufferedEventBytes -= pending.event.kind === "output" ? pending.event.data.length : 0;
        if (!commitRecord(pending)) break;
      }
      void drainRecords();
    };

    const addPageRecords = (records: NativeTerminalOutputRecord[]) => {
      if (records.length > MAX_BUFFERED_EVENTS) {
        disconnect("Native terminal replay page exceeded the safe event limit.");
        return;
      }
      const ordered = [...records].sort((left, right) => {
        const a = BigInt(left.sequence);
        const b = BigInt(right.sequence);
        return a < b ? -1 : a > b ? 1 : 0;
      });
      for (const record of ordered) {
        if (disconnected || cancelled) return;
        acceptRecord(record);
      }
    };

    async function drainRecords() {
      if (draining || disconnected || cancelled || !attached || waitingForRendererReplay()) return;
      draining = true;
      try {
        while (!disconnected && !cancelled) {
          const nextExpected = lastSequence + 1n;
          const hasLaterRecord = [...pendingRecords.keys()].some(
            (sequence) => sequence > nextExpected,
          );
          if (!hasLaterRecord && nextExpected >= targetSequence) break;

          const page = await nativeTerminalOutput({
            sessionId: sessionId ?? "",
            afterSequence: lastSequence.toString(),
          });
          if (cancelled) return;
          if (waitingForRendererReplay()) return;
          const sequenceBeforePage = lastSequence;
          addPageRecords(page.events);
          if (disconnected) break;
          const pageNext = BigInt(page.nextSequence);
          if (pageNext > targetSequence) targetSequence = pageNext;

          if (lastSequence === sequenceBeforePage) {
            if (BigInt(page.nextSequence) > lastSequence + 1n || hasLaterRecord) {
              disconnect("Native terminal output sequence has a gap; replay stopped.");
            }
            break;
          }
        }
      } catch (error) {
        if (!cancelled) disconnect(errorMessage(error));
      } finally {
        draining = false;
        if (!cancelled && !disconnected && pendingRecords.has(lastSequence + 1n)) {
          void drainRecords();
        }
      }
    }

    const onNativeEvent = (payload: unknown) => {
      if (cancelled || disconnected || waitingForRendererReplay()) return;
      const event = parseNativeTerminalOutputEvent(payload);
      if (!event || event.record.sessionId !== sessionId) return;
      if (
        subscriptionId !== null &&
        event.subscriptionId !== null &&
        event.subscriptionId !== subscriptionId
      ) {
        return;
      }
      if (!attached) {
        if (
          earlyEvents.length >= MAX_BUFFERED_EVENTS ||
          bufferedEventBytes +
            (event.record.event.kind === "output" ? event.record.event.data.length : 0) >
            MAX_BUFFERED_OUTPUT_BYTES
        ) {
          disconnect("Native terminal event backlog exceeded the safe attach limit.");
          return;
        }
        earlyEvents.push(event);
        bufferedEventBytes +=
          event.record.event.kind === "output" ? event.record.event.data.length : 0;
        return;
      }
      acceptRecord(event.record);
    };

    if (!sessionId) {
      const empty: Snapshot = {
        session: null,
        ownsSession: false,
        feedStatus: "unavailable",
        error: null,
        bridge,
      };
      snapshotRef.current = empty;
      setSnapshot(empty);
      return;
    }

    const initial: Snapshot = {
      session: null,
      ownsSession: false,
      feedStatus: "unavailable",
      error: detachErrors.current.get(sessionId) ?? null,
      bridge,
    };
    snapshotRef.current = initial;
    setSnapshot(initial);

    const connect = async () => {
      try {
        const release = await listen<unknown>("native-terminal-output", (event) => {
          onNativeEvent(event.payload);
        });
        if (cancelled) {
          try {
            release();
          } catch {
            // A failed WebView release must not imply that the worker stopped.
          }
          return;
        }
        releaseListener = release;
        const attachment = await nativeTerminalAttach({
          sessionId,
          afterSequence: lastSequence.toString(),
        });
        if (cancelled) {
          if (attachment.subscriptionId !== null) {
            // A late attach owns a native receiver until it is explicitly detached.
            void detachSubscription(sessionId, attachment.subscriptionId).catch(
              reportDetachFailure,
            );
          }
          return;
        }
        subscriptionId = attachment.subscriptionId;
        if (attachment.session.sessionId !== sessionId) {
          throw new Error("native terminal attach returned a different session");
        }
        if (rendererReplayGenerations.current.get(sessionId) === generation) {
          rendererReplayCursors.current.delete(sessionId);
          rendererReplayGenerations.current.delete(sessionId);
        }
        attached = true;
        const initialEvents = [...attachment.events].sort((left, right) => {
          const a = BigInt(left.sequence);
          const b = BigInt(right.sequence);
          return a < b ? -1 : a > b ? 1 : 0;
        });
        targetSequence = BigInt(attachment.nextSequence);
        const listed = listedSessionRef.current;
        const listedState = listed?.sessionId === sessionId ? listed : null;
        const sessionMerge = mergeNativeTerminalSessionFacts(listedState, attachment.session);
        const listedBlocksInput =
          listedState !== null && hasNativeTerminalInputBlocker(listedState);
        const attachmentBlocksInput = hasNativeTerminalInputBlocker(attachment.session);
        const currentFeed: TerminalFeedStatus = disconnected
          ? "disconnected"
          : attachment.session.state === "unknown" ||
              listedState?.state === "unknown" ||
              sessionMerge.protocolError !== null
            ? "unknown"
            : attachment.subscriptionId !== null &&
                attachment.session.state === "running" &&
                !attachmentBlocksInput &&
                !listedBlocksInput
              ? "active"
              : "unavailable";
        const canTrustInput =
          currentFeed === "active" &&
          attachment.session.state === "running" &&
          !attachmentBlocksInput &&
          !listedBlocksInput &&
          sessionMerge.protocolError === null;
        inputPermissionRef.current = {
          sessionId,
          generation,
          blocked: !canTrustInput,
        };
        lastSequences.current.set(sessionId, lastSequence);
        const attachError = snapshotRef.current.error;
        setCurrent({
          session: attachment.session,
          ownsSession: attachment.subscriptionId !== null,
          feedStatus: currentFeed,
          error: disconnected
            ? attachError
            : (sessionMerge.protocolError ?? detachErrors.current.get(sessionId) ?? null),
        });
        addPageRecords(initialEvents);
        for (const event of earlyEvents) {
          const bytes = event.record.event.kind === "output" ? event.record.event.data.length : 0;
          bufferedEventBytes = Math.max(0, bufferedEventBytes - bytes);
          if (
            subscriptionId === null ||
            event.subscriptionId === null ||
            event.subscriptionId === subscriptionId
          ) {
            acceptRecord(event.record);
          }
        }
        earlyEvents.length = 0;
        void drainRecords();
      } catch (error) {
        if (!cancelled) {
          if (subscriptionId !== null) {
            void detachSubscription(sessionId, subscriptionId).catch(reportDetachFailure);
          }
          const release = releaseListener;
          releaseListener = null;
          try {
            release?.();
          } catch {
            // A failed WebView release must not imply that the worker stopped.
          }
          setCurrent({
            session: null,
            ownsSession: false,
            feedStatus: "unavailable",
            error: errorMessage(error),
          });
        }
      }
    };

    void connect();

    return () => {
      cancelled = true;
      blockCurrentInput();
      if (subscriptionId !== null) {
        void detachSubscription(sessionId, subscriptionId).catch(reportDetachFailure);
      }
      const release = releaseListener;
      releaseListener = null;
      try {
        release?.();
      } catch {
        // Detaching the WebView listener must not cancel the native process.
      }
      earlyEvents.length = 0;
      pendingRecords.clear();
      if (sessionId && selectedSessionIdRef.current !== sessionId) {
        lastSequences.current.delete(sessionId);
        pendingOutput.current.delete(sessionId);
        outputListeners.current.delete(sessionId);
        rendererBaselines.current.delete(sessionId);
        rendererReplayCursors.current.delete(sessionId);
        rendererReplayGenerations.current.delete(sessionId);
      }
    };
  }, [attachRevision, bridge, sessionId]);

  return { ...snapshot, bridge, reattach };
}
