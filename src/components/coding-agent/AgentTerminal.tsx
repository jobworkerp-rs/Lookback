import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import "@xterm/xterm/css/xterm.css";
import { installImeCommitBridge } from "./terminalImeInput";
import type {
  AgentSessionStatus,
  AgentTerminalBridge,
  AgentTerminalSession,
  TerminalRenderer,
  TerminalRendererFactory,
  TerminalRendererOptions,
  TerminalRendererSubscription,
} from "./types";

export interface AgentTerminalProps {
  bridge: AgentTerminalBridge;
  session: AgentTerminalSession;
  screenReaderMode?: boolean;
  rendererFactory?: TerminalRendererFactory;
  onCancelRequested?: (runId: string) => void | Promise<void>;
  /** Requests normal finish/finalization; it does not approve an Issue as DONE. */
  onFinishRequested?: (runId: string) => void | Promise<void>;
  /** Backend-derived permission: true only for a normal exit or supported graceful finish request. */
  canFinish?: boolean;
  /** The parent owns user approval and the platform's external-open policy. */
  onRequestOpenLink?: (url: string) => void;
}

type TerminalAction = "cancel" | "finish";

interface TerminalActionState {
  runId: string;
  token: number;
  pendingAction: TerminalAction | null;
  error: string | null;
}

const TERMINAL_COLUMNS = 80;
const TERMINAL_ROWS = 24;

const statusFallback: Record<AgentSessionStatus, string> = {
  preparing: "実行環境を準備中",
  starting: "端末を起動中",
  ready: "端末入力の準備完了",
  running: "作業中",
  cancelling: "取消要求中",
  finished: "端末作業終了",
  failed: "端末作業失敗",
  cancelled: "端末作業取消済み",
  disconnected: "端末接続切断",
  unknown: "端末状態不明",
};

const defaultRendererFactory: TerminalRendererFactory = async ({
  screenReaderMode,
  onLinkActivation,
}: TerminalRendererOptions): Promise<TerminalRenderer> => {
  const [{ Terminal }, { FitAddon }] = await Promise.all([
    import("@xterm/xterm"),
    import("@xterm/addon-fit"),
  ]);
  const terminal = new Terminal({
    cols: TERMINAL_COLUMNS,
    rows: TERMINAL_ROWS,
    screenReaderMode,
    allowProposedApi: false,
    linkHandler: {
      allowNonHttpProtocols: false,
      activate: (_event, text) => onLinkActivation(text),
    },
  });
  const fitAddon = new FitAddon();
  terminal.loadAddon(fitAddon);
  const imeListeners = new Set<(data: string) => void>();
  let disposeImeBridge: (() => void) | null = null;

  return {
    open: (container) => {
      terminal.open(container);
      disposeImeBridge = installImeCommitBridge(
        container,
        (data) => {
          for (const listener of imeListeners) listener(data);
        },
        () => !terminal.options.disableStdin,
      );
    },
    write: (bytes) => terminal.write(bytes),
    onData: (listener) => {
      const subscription = terminal.onData(listener);
      imeListeners.add(listener);
      return {
        dispose: () => {
          imeListeners.delete(listener);
          subscription.dispose();
        },
      };
    },
    onBinary: (listener) => terminal.onBinary(listener),
    setInputEnabled: (enabled) => {
      terminal.options.disableStdin = !enabled;
    },
    // Terminal.dispose also unloads its addons, including FitAddon.
    dispose: () => {
      disposeImeBridge?.();
      disposeImeBridge = null;
      imeListeners.clear();
      terminal.dispose();
    },
  };
};

export function validateTerminalLink(rawUrl: string): string | null {
  if (rawUrl.trim() !== rawUrl) return null;
  if (
    Array.from(rawUrl).some((character) => {
      const codePoint = character.codePointAt(0) ?? 0;
      return codePoint <= 0x1f || codePoint === 0x7f;
    })
  ) {
    return null;
  }

  try {
    const url = new URL(rawUrl);
    if (
      (url.protocol !== "http:" && url.protocol !== "https:") ||
      !url.hostname ||
      url.username.length > 0 ||
      url.password.length > 0
    ) {
      return null;
    }
    return url.toString();
  } catch {
    return null;
  }
}

function ownsActiveSession(session: AgentTerminalSession): boolean {
  return session.ownsSession && (session.status === "ready" || session.status === "running");
}

function canInput(session: AgentTerminalSession): boolean {
  return ownsActiveSession(session) && session.feedStatus === "active";
}

function canRequestFinish(session: AgentTerminalSession, canFinish: boolean): boolean {
  return (
    canFinish &&
    session.ownsSession &&
    session.purpose === "interactive-run" &&
    (session.status === "ready" || session.status === "running" || session.status === "finished")
  );
}

function feedStatusText(
  feedStatus: AgentTerminalSession["feedStatus"],
  t: ReturnType<typeof useTranslation>["t"],
): string {
  const messages = {
    active:
      "アプリ管理の端末フィードは接続中です。ウィンドウを閉じても描画器だけが切り離され、実行の取消にはなりません。",
    disconnected:
      "アプリ管理の端末フィードが切断されています。成功・終了を示さず、入力再接続や出力replayも保証しません。",
    unavailable: "端末フィードは利用できません。実行結果や終了状態を推測しません。",
    unknown: "端末フィードの状態は不明です。成功・終了とは扱いません。",
  } as const;
  return t(`codingAgent.terminal.feed.${feedStatus}`, { defaultValue: messages[feedStatus] });
}

export function AgentTerminal({
  bridge,
  session,
  screenReaderMode = true,
  rendererFactory = defaultRendererFactory,
  onCancelRequested,
  onFinishRequested,
  canFinish = false,
  onRequestOpenLink,
}: AgentTerminalProps) {
  const { t } = useTranslation();
  const hostRef = useRef<HTMLDivElement>(null);
  const rendererRef = useRef<TerminalRenderer | null>(null);
  const propsRef = useRef({
    bridge,
    session,
    onCancelRequested,
    onFinishRequested,
    canFinish,
    onRequestOpenLink,
  });
  propsRef.current = {
    bridge,
    session,
    onCancelRequested,
    onFinishRequested,
    canFinish,
    onRequestOpenLink,
  };

  const [actionState, setActionState] = useState<TerminalActionState>({
    runId: session.runId,
    token: 0,
    pendingAction: null,
    error: null,
  });
  const actionToken = useRef(0);
  const mountedRef = useRef(true);
  const [inputErrorRunId, setInputErrorRunId] = useState<string | null>(null);
  const [rendererErrorRunId, setRendererErrorRunId] = useState<string | null>(null);
  const rendererUnavailable = rendererErrorRunId === session.runId;
  const inputEnabled = canInput(session);
  const inputUnavailable = inputErrorRunId === session.runId;
  const currentActionState = actionState.runId === session.runId ? actionState : null;
  const pendingAction = currentActionState?.pendingAction ?? null;
  const actionError = currentActionState?.error ?? null;

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    setActionState({ runId: session.runId, token: 0, pendingAction: null, error: null });
  }, [session.runId]);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    setRendererErrorRunId(null);

    let attached = true;
    let rendererActive = false;
    let resourcesReleased = false;
    let renderer: TerminalRenderer | null = null;
    let unsubscribeOutput: (() => void) | null = null;
    let dataSubscription: TerminalRendererSubscription | null = null;
    let binarySubscription: TerminalRendererSubscription | null = null;

    const releaseRendererResources = () => {
      if (resourcesReleased) return;
      resourcesReleased = true;
      rendererActive = false;
      try {
        unsubscribeOutput?.();
      } catch {
        // Renderer teardown must continue even if an adapter unsubscribe fails.
      }
      unsubscribeOutput = null;
      try {
        dataSubscription?.dispose();
      } catch {
        // Renderer teardown must continue even if an event subscription fails to dispose.
      }
      dataSubscription = null;
      try {
        binarySubscription?.dispose();
      } catch {
        // Renderer teardown must continue even if an event subscription fails to dispose.
      }
      binarySubscription = null;
      const releasedRenderer = renderer;
      renderer = null;
      if (rendererRef.current === releasedRenderer) rendererRef.current = null;
      try {
        releasedRenderer?.dispose();
      } catch {
        // A renderer failure is already visible; it must not affect app-owned job lifetime.
      }
    };

    const sendInput = (bytes: Uint8Array) => {
      const current = propsRef.current;
      if (!attached || !rendererActive || !canInput(current.session)) return;

      try {
        void Promise.resolve(current.bridge.sendInput(current.session.runId, bytes)).catch(() => {
          if (propsRef.current.session.runId === current.session.runId) {
            setInputErrorRunId(current.session.runId);
          }
        });
      } catch {
        setInputErrorRunId(propsRef.current.session.runId);
      }
    };

    const installRenderer = (created: TerminalRenderer) => {
      if (!attached) {
        try {
          created.dispose();
        } catch {
          // The terminal was already detached; no backend action is implied.
        }
        return;
      }
      renderer = created;
      rendererRef.current = created;
      try {
        created.open(host);
        created.setInputEnabled(canInput(propsRef.current.session));
        rendererActive = true;
        let subscribing = true;
        let outputFailed = false;
        unsubscribeOutput = bridge.subscribeOutput(session.runId, (bytes) => {
          if (!attached || !rendererActive) return;
          try {
            created.write(bytes);
          } catch {
            outputFailed = true;
            rendererActive = false;
            // A synchronous notification must return so we can obtain its unsubscribe handle.
            if (!subscribing) releaseRendererResources();
            setRendererErrorRunId(session.runId);
          }
        });
        subscribing = false;
        if (outputFailed) {
          releaseRendererResources();
          return;
        }
        dataSubscription = created.onData((data) => {
          if (rendererActive && canInput(propsRef.current.session)) {
            sendInput(new TextEncoder().encode(data));
          }
        });
        binarySubscription = created.onBinary((data) => {
          if (rendererActive && canInput(propsRef.current.session)) {
            sendInput(Uint8Array.from(data, (character) => character.charCodeAt(0) & 0xff));
          }
        });
        setRendererErrorRunId(null);
      } catch {
        releaseRendererResources();
        setRendererErrorRunId(session.runId);
      }
    };

    try {
      const result = rendererFactory({
        screenReaderMode,
        onLinkActivation: (rawUrl) => {
          if (!attached || !rendererActive) return;
          const safeUrl = validateTerminalLink(rawUrl);
          if (safeUrl) propsRef.current.onRequestOpenLink?.(safeUrl);
        },
      });
      if (result instanceof Promise) {
        void result.then(installRenderer).catch(() => {
          if (attached) setRendererErrorRunId(session.runId);
        });
      } else {
        installRenderer(result);
      }
    } catch {
      releaseRendererResources();
      setRendererErrorRunId(session.runId);
    }

    return () => {
      attached = false;
      releaseRendererResources();
    };
  }, [bridge, rendererFactory, screenReaderMode, session.runId]);

  useEffect(() => {
    rendererRef.current?.setInputEnabled(inputEnabled);
  }, [inputEnabled]);

  const requestAction = (action: TerminalAction) => {
    const current = propsRef.current;
    if (actionState.runId === current.session.runId && actionState.pendingAction !== null) return;
    if (action === "cancel" && !ownsActiveSession(current.session)) return;
    if (action === "finish" && !canRequestFinish(current.session, current.canFinish)) return;

    const handler = action === "cancel" ? current.onCancelRequested : current.onFinishRequested;
    if (!handler) return;

    const runId = current.session.runId;
    const token = ++actionToken.current;
    setActionState({ runId, token, pendingAction: action, error: null });
    const settle = (failed: boolean) => {
      if (!mountedRef.current || propsRef.current.session.runId !== runId) return;
      setActionState((currentState) => {
        if (currentState.runId !== runId || currentState.token !== token) return currentState;
        return {
          runId,
          token,
          pendingAction: null,
          error: failed
            ? t("codingAgent.terminal.actionError", {
                defaultValue: "操作結果を確認できません。実行状態を照合してください。",
              })
            : null,
        };
      });
    };
    try {
      void Promise.resolve(handler(runId)).then(
        () => settle(false),
        () => settle(true),
      );
    } catch {
      settle(true);
    }
  };

  const canRequestCancel = ownsActiveSession(session);
  const canFinishForSession = canRequestFinish(session, canFinish);
  const sessionLabel = t(`codingAgent.terminal.status.${session.status}`, {
    defaultValue: statusFallback[session.status],
  });
  const terminalLabel = t("codingAgent.terminal.title", { defaultValue: "Coding agent 端末" });
  const dimensionWarning = t("codingAgent.terminal.dimensions", {
    defaultValue:
      "表示は80列×24行固定です。画面サイズに合わせたfitやバックエンドPTY resizeは行いません。入力再接続・出力replayも未対応です。",
  });

  return (
    <section
      aria-label={terminalLabel}
      data-run-id={session.runId}
      data-job-id={session.jobId ?? undefined}
      style={{ display: "grid", gap: 8, minWidth: 0 }}
    >
      <div style={{ display: "flex", flexWrap: "wrap", gap: 8, alignItems: "center" }}>
        <strong>{sessionLabel}</strong>
        <span style={{ color: "var(--label-secondary)", fontSize: 11 }}>
          {session.runId}
          {session.jobId ? ` · ${session.jobId}` : ""}
        </span>
      </div>
      <div
        role="status"
        aria-live="polite"
        style={{ color: "var(--label-secondary)", fontSize: 11 }}
      >
        {feedStatusText(session.feedStatus, t)}
      </div>
      <section
        ref={hostRef}
        aria-label={t("codingAgent.terminal.viewport", { defaultValue: "端末表示領域" })}
        style={{
          minWidth: 0,
          minHeight: 240,
          overflow: "auto",
          padding: 8,
          borderRadius: 6,
          background: "#111",
        }}
      />
      {rendererUnavailable && (
        <p role="alert" style={{ color: "var(--danger)", fontSize: 11 }}>
          {t("codingAgent.terminal.rendererError", {
            defaultValue: "端末表示器を初期化できませんでした。実行自体は取消されていません。",
          })}
        </p>
      )}
      <p style={{ color: "var(--label-secondary)", fontSize: 11 }}>{dimensionWarning}</p>
      {inputUnavailable && (
        <p role="alert" style={{ color: "var(--danger)", fontSize: 11 }}>
          {t("codingAgent.terminal.inputError", {
            defaultValue:
              "端末入力feedへ書き込めませんでした。再送せず、接続状態を確認してください。",
          })}
        </p>
      )}
      {actionError && (
        <p role="alert" style={{ color: "var(--danger)", fontSize: 11 }}>
          {actionError}
        </p>
      )}
      <div style={{ display: "flex", flexWrap: "wrap", gap: 8 }}>
        <button
          type="button"
          className="btn"
          disabled={!canRequestCancel || !onCancelRequested || pendingAction !== null}
          onClick={() => requestAction("cancel")}
        >
          {pendingAction === "cancel"
            ? t("codingAgent.terminal.cancelling", { defaultValue: "取消要求中…" })
            : t("codingAgent.terminal.cancel", { defaultValue: "取消" })}
        </button>
        {session.purpose === "interactive-run" && (
          <button
            type="button"
            className="btn"
            disabled={!canFinishForSession || !onFinishRequested || pendingAction !== null}
            onClick={() => requestAction("finish")}
          >
            {pendingAction === "finish"
              ? t("codingAgent.terminal.finishing", { defaultValue: "終了要求中…" })
              : t("codingAgent.terminal.finish", {
                  defaultValue: "作業を終了して検証・公開へ",
                })}
          </button>
        )}
        <span style={{ color: "var(--label-secondary)", fontSize: 11, alignSelf: "center" }}>
          {t("codingAgent.terminal.cancelIsSeparate", {
            defaultValue: "取消は停止要求です。finishや完了承認とは別操作です。",
          })}
        </span>
      </div>
    </section>
  );
}
