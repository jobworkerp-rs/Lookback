import type { NativeTerminalSession } from "@/api/nativeTerminal";
import type { AgentTerminalBridge, AgentTerminalSession, TerminalFeedStatus } from "./types";

export interface NativeTerminalSessionMerge {
  session: NativeTerminalSession | null;
  protocolError: string | null;
}

/** Native lifecycle/output facts accumulate; stale snapshots cannot erase them. */
export function mergeNativeTerminalSessionFacts(
  listed: NativeTerminalSession | null,
  attached: NativeTerminalSession | null,
): NativeTerminalSessionMerge {
  if (listed === null) return { session: attached, protocolError: null };
  if (attached === null) return { session: listed, protocolError: null };
  if (listed.sessionId !== attached.sessionId) {
    return {
      session: null,
      protocolError: "Native terminal returned a different session ID.",
    };
  }

  const conflicts: string[] = [];
  if (listed.jobId !== null && attached.jobId !== null && listed.jobId !== attached.jobId) {
    conflicts.push("job ID");
  }
  if (
    listed.exitCode !== null &&
    attached.exitCode !== null &&
    listed.exitCode !== attached.exitCode
  ) {
    conflicts.push("exit code");
  }
  if (listed.issueId !== attached.issueId) conflicts.push("Issue owner");
  if (listed.previewId !== attached.previewId) conflicts.push("preview ID");
  if (listed.profileId !== attached.profileId) conflicts.push("profile ID");

  const finalStates = new Set(["closed", "cancelled"]);
  const listedFinal = finalStates.has(listed.state);
  const attachedFinal = finalStates.has(attached.state);
  if (listedFinal && attachedFinal && listed.state !== attached.state) {
    conflicts.push("final state");
  }

  const state: NativeTerminalSession["state"] =
    conflicts.length > 0
      ? "unknown"
      : listedFinal || attachedFinal
        ? listedFinal
          ? listed.state
          : attached.state
        : listed.state === "unknown" || attached.state === "unknown"
          ? "unknown"
          : listed.state === "running" || attached.state === "running"
            ? "running"
            : "starting";

  return {
    session: {
      ...listed,
      ...attached,
      state,
      jobId: conflicts.includes("job ID") ? null : (attached.jobId ?? listed.jobId),
      exitCode: conflicts.includes("exit code") ? null : (attached.exitCode ?? listed.exitCode),
      endReceived: listed.endReceived || attached.endReceived,
      streamEof: listed.streamEof || attached.streamEof,
      streamError: attached.streamError ?? listed.streamError,
      cancelRequested: listed.cancelRequested || attached.cancelRequested,
    },
    protocolError:
      conflicts.length > 0 ? `Conflicting native session facts: ${conflicts.join(", ")}.` : null,
  };
}

export function hasNativeTerminalInputBlocker(session: NativeTerminalSession): boolean {
  return (
    session.state === "unknown" ||
    session.state === "closed" ||
    session.state === "cancelled" ||
    session.cancelRequested ||
    session.exitCode !== null ||
    session.endReceived ||
    session.streamEof ||
    session.streamError !== null
  );
}

export interface NativeTerminalBridgeState {
  session: NativeTerminalSession | null;
  ownsSession: boolean;
  feedStatus: TerminalFeedStatus;
  error: string | null;
  bridge: AgentTerminalBridge;
  /** Starts a fresh native output attachment; it does not restart the process. */
  reattach: () => void;
}

/**
 * AgentTerminal only exposes finish-work for interactive-run sessions. Native
 * sessions use the non-finalizing presentation mode; their IO still stays on
 * the native bridge and is not routed through the authentication flow.
 */
export function toAgentTerminalSession(
  session: NativeTerminalSession,
  ownsSession: boolean,
  feedStatus: TerminalFeedStatus,
): AgentTerminalSession {
  const terminalEnded =
    session.exitCode !== null ||
    (session.state !== "unknown" && session.endReceived && session.streamEof);
  const status: AgentTerminalSession["status"] = terminalEnded
    ? "finished"
    : session.cancelRequested
      ? "cancelling"
      : session.state === "closed"
        ? "finished"
        : session.state === "cancelled"
          ? "cancelled"
          : session.state;

  return {
    runId: session.sessionId,
    jobId: session.jobId,
    purpose: "authentication",
    status,
    ownsSession,
    feedStatus,
  };
}
