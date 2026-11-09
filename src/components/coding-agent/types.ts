/** `ready` means the profile-confirmed agent prompt input is ready, not just shell startup. */
export type AgentSessionStatus =
  | "preparing"
  | "starting"
  | "ready"
  | "running"
  | "cancelling"
  | "finished"
  | "failed"
  | "cancelled"
  | "disconnected"
  | "unknown";

export type TerminalFeedStatus = "active" | "disconnected" | "unavailable" | "unknown";

export interface AgentTerminalSession {
  runId: string;
  jobId?: string | null;
  /** Authentication sessions accept input but adapters must exclude their traffic from run logs. */
  purpose: "interactive-run" | "authentication";
  status: AgentSessionStatus;
  ownsSession: boolean;
  /** App-owner feed status, independent from this component's renderer attachment. */
  feedStatus: TerminalFeedStatus;
}

export type Unsubscribe = () => void;

/**
 * The adapter owns the feed and job. Unsubscribe detaches this renderer only;
 * it must not stop the app-owned reader or cancel an active job.
 * Authentication purpose must keep this traffic out of ordinary Issue run logs.
 */
export interface AgentTerminalBridge {
  subscribeOutput(runId: string, onOutput: (bytes: Uint8Array) => void): Unsubscribe;
  sendInput(runId: string, bytes: Uint8Array): void | Promise<void>;
}

export interface TerminalRendererSubscription {
  dispose(): void;
}

export interface TerminalRenderer {
  open(container: HTMLElement): void;
  write(bytes: Uint8Array): void;
  onData(listener: (data: string) => void): TerminalRendererSubscription;
  onBinary(listener: (data: string) => void): TerminalRendererSubscription;
  setInputEnabled(enabled: boolean): void;
  dispose(): void;
}

export interface TerminalRendererOptions {
  screenReaderMode: boolean;
  /** Called only for a user-activated OSC 8 link after HTTP(S) validation. */
  onLinkActivation(rawUrl: string): void;
}

export type TerminalRendererFactory = (
  options: TerminalRendererOptions,
) => TerminalRenderer | Promise<TerminalRenderer>;
