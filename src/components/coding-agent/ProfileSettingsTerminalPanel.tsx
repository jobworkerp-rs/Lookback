import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import {
  type ProfileTerminalSession,
  profileTerminalCancel,
  profileTerminalFinishInput,
  profileTerminalList,
  profileTerminalStart,
} from "@/api/profileTerminal";
import { SettingsHint } from "@/components/SettingsHint";
import { AgentTerminal } from "./AgentTerminal";
import type { AgentTerminalSession, TerminalRendererFactory } from "./types";
import { useProfileSettingsTerminalBridge } from "./useProfileSettingsTerminalBridge";

function terminalGuideKey(
  command: readonly string[],
): "terminalGuideCodex" | "terminalGuideClaude" | "terminalGuideOpenCode" | "terminalGuideGeneric" {
  const executable = command
    .find((token) => token.trim())
    ?.trim()
    .split(/\s+/)[0]
    ?.split(/[\\/]/)
    .at(-1)
    ?.toLowerCase();
  if (executable === "codex") return "terminalGuideCodex";
  if (executable === "claude") return "terminalGuideClaude";
  if (executable === "opencode") return "terminalGuideOpenCode";
  return "terminalGuideGeneric";
}

export function ProfileSettingsTerminalPanel({
  profileId,
  revision,
  dirty,
  saving = false,
  credentialReference = null,
  homeReady = true,
  command = [],
  rendererFactory,
}: {
  profileId: string;
  revision: number;
  dirty: boolean;
  saving?: boolean;
  credentialReference?: string | null;
  /** False when the profile has no writable home mount yet; the backend would refuse the shell. */
  homeReady?: boolean;
  command?: readonly string[];
  rendererFactory?: TerminalRendererFactory;
}) {
  const { t } = useTranslation();
  const list = useQuery({
    queryKey: ["profile-settings-terminals"],
    queryFn: profileTerminalList,
    retry: false,
    refetchInterval: 1000,
  });
  const [started, setStarted] = useState<ProfileTerminalSession | null>(null);
  const [starting, setStarting] = useState(false);
  const [finishing, setFinishing] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const sessions = list.data?.filter((session) => session.profileId === profileId) ?? [];
  const listed =
    [...sessions].reverse().find((session) => session.state !== "closed") ??
    sessions.at(-1) ??
    null;
  const selected =
    started?.profileId === profileId
      ? (sessions.find((session) => session.sessionId === started.sessionId) ?? started)
      : listed;
  const feed = useProfileSettingsTerminalBridge(selected?.sessionId ?? null);
  const session =
    selected?.state === "closed" || feed.session?.sessionId !== selected?.sessionId
      ? selected
      : (feed.session ?? selected);
  const active = session && session.state !== "closed";
  const canInput =
    session?.state === "running" && !session.inputClosed && feed.feedStatus === "active";
  const blockedReason = credentialReference
    ? t("codingAgent.settings.terminalCredentialReference")
    : !homeReady
      ? t("codingAgent.settings.terminalHomeMissing")
      : saving
        ? t("codingAgent.settings.terminalSaving")
        : dirty
          ? t("codingAgent.settings.terminalDirty")
          : starting
            ? t("codingAgent.settings.terminalStarting")
            : list.isPending
              ? t("codingAgent.settings.terminalListPending")
              : list.isError
                ? t("codingAgent.settings.terminalListError")
                : session?.state === "unknown"
                  ? t("codingAgent.settings.terminalUnknownSession")
                  : active
                    ? t("codingAgent.settings.terminalActiveSession")
                    : null;

  const start = async () => {
    if (
      credentialReference ||
      !homeReady ||
      dirty ||
      saving ||
      starting ||
      active ||
      list.isPending ||
      list.isError
    )
      return;
    setStarting(true);
    setError(null);
    try {
      setStarted(await profileTerminalStart({ expectedRevision: revision, profileId }));
    } catch {
      setError(t("codingAgent.settings.terminalStartError"));
    } finally {
      await list.refetch();
      setStarting(false);
    }
  };
  const finish = async () => {
    if (!session || !canInput || finishing) return;
    setFinishing(true);
    try {
      await profileTerminalFinishInput(session.sessionId);
    } catch {
      setError(t("codingAgent.settings.terminalOperationError"));
    } finally {
      setFinishing(false);
      await list.refetch();
    }
  };
  const cancelUnknown = async () => {
    if (session?.state !== "unknown" || !session.jobId || cancelling) return;
    setCancelling(true);
    try {
      await profileTerminalCancel(session.sessionId);
    } catch {
      setError(t("codingAgent.settings.terminalOperationError"));
    } finally {
      await list.refetch();
      setCancelling(false);
    }
  };
  const terminalSession: AgentTerminalSession | null = session
    ? {
        runId: session.sessionId,
        jobId: session.jobId,
        purpose: "authentication",
        ownsSession: true,
        feedStatus: feed.feedStatus,
        status:
          session.state === "closed"
            ? session.exitCode === 0
              ? "finished"
              : "failed"
            : session.state === "running"
              ? "running"
              : session.state === "cancelling"
                ? "cancelling"
                : session.state === "starting"
                  ? "starting"
                  : "unknown",
      }
    : null;
  return (
    <section aria-label={t("codingAgent.settings.settingsTerminal")}>
      <div className="coding-agent-settings-panel-toolbar">
        <h3>{t("codingAgent.settings.settingsTerminal")}</h3>
        <SettingsHint
          label={t("codingAgent.settings.terminalHelpLabel")}
          content={t("codingAgent.settings.terminalDescription")}
        />
      </div>
      <p>{t("codingAgent.settings.terminalDescription")}</p>
      <p>{t("codingAgent.settings.terminalReuseDescription")}</p>
      <p>{t(`codingAgent.settings.${terminalGuideKey(command)}`)}</p>
      {blockedReason && <p role="status">{blockedReason}</p>}
      <button
        type="button"
        className="btn"
        disabled={
          Boolean(credentialReference) ||
          !homeReady ||
          dirty ||
          saving ||
          starting ||
          Boolean(active) ||
          list.isPending ||
          list.isError
        }
        onClick={() => void start()}
      >
        {t("codingAgent.settings.openTerminal")}
      </button>
      {list.isError && (
        <button type="button" className="btn" onClick={() => void list.refetch()}>
          {t("codingAgent.settings.retry")}
        </button>
      )}
      {terminalSession && (
        <>
          <AgentTerminal
            bridge={feed.bridge}
            session={terminalSession}
            rendererFactory={rendererFactory}
            onCancelRequested={async (id) => {
              await profileTerminalCancel(id);
              await list.refetch();
            }}
          />
          {session?.state === "unknown" && session.jobId && (
            <button
              type="button"
              className="btn"
              disabled={cancelling}
              onClick={() => void cancelUnknown()}
            >
              {t("codingAgent.settings.cancelUnknownTerminal")}
            </button>
          )}
          <button
            type="button"
            className="btn"
            disabled={!canInput || finishing}
            onClick={() => void finish()}
          >
            {t("codingAgent.settings.finishTerminalInput")}
          </button>
          {feed.feedStatus === "disconnected" && (
            <button type="button" className="btn" onClick={feed.reattach}>
              {t("codingAgent.settings.reattachTerminal")}
            </button>
          )}
        </>
      )}
      {(error || feed.error || list.isError) && (
        <p role="alert">{error ?? t("codingAgent.settings.terminalOperationError")}</p>
      )}
    </section>
  );
}
