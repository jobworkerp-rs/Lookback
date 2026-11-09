import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  type NativeTerminalPreview,
  type NativeTerminalSession,
  nativeTerminalCancel,
  nativeTerminalList,
  nativeTerminalPrepare,
  nativeTerminalStart,
  nativeTerminalStatus,
} from "@/api/nativeTerminal";
import { AgentTerminal } from "./AgentTerminal";
import {
  hasNativeTerminalInputBlocker,
  mergeNativeTerminalSessionFacts,
  toAgentTerminalSession,
} from "./nativeTerminalTypes";
import { useNativeTerminalBridge } from "./useNativeTerminalBridge";

interface PreviewSnapshot {
  formKey: string;
  editGeneration: number;
  statusGeneration: number;
  preview: NativeTerminalPreview;
  startAttempted: boolean;
}

interface SelectedSession {
  issueId: string;
  sessionId: string;
}

const text = {
  title: "Native terminal セッション",
  description:
    "準備済みの native profile で CLI の対話端末を起動します。成功・Issue 完了は判定しません。",
  checking: "Native 実行環境を確認中…",
  statusUnavailable: "Native 実行環境の状態を取得できません。",
  unavailable: "Native 実行環境を利用できません。",
  noProfiles: "利用可能な profile がありません。",
  profile: "Native profile",
  chooseProfile: "profile を選択",
  baseBranch: "ベースブランチ",
  chooseBranch: "標準branch（remote HEAD）",
  noAllowedBranches: "許可された base branch がありません。自動選択はしません。",
  initialPrompt: "初回プロンプト",
  promptByteLimit:
    "初回プロンプトは UTF-8 64 KiB 以下にしてください。切り捨てません。argv 全体の制限により preview が拒否される場合もあります。",
  promptNulRejected: "NUL 文字は初回プロンプトに使用できません。",
  prepare: "実行環境を準備",
  preparing: "準備中…",
  preview: "実行プレビュー",
  previousPreview: "以前のプレビュー",
  previewStale: "入力が変わりました。新しいプレビューを作成するまで開始できません。",
  repository: "Repository",
  previewId: "Preview ID",
  previewDigest: "Preview digest",
  settingsRevision: "Settings revision",
  issueRevision: "Issue revision",
  branch: "Base branch",
  baseCommit: "Base commit",
  cliVersion: "期待する CLI version",
  imageDigest: "Safe image digest",
  command: "実行 argv",
  workingDirectory: "Working directory",
  runtimeNotObserved: "Native runtime の観測を確認できません。起動はブロックされています。",
  startAttempted: "起動要求を試行しました。状態を再取得してください。再送はしません。",
  start: "端末を起動",
  starting: "起動要求中…",
  sessionsUnavailable: "保存済みセッション一覧を取得できません。",
  savedSessions: "保存済み native session",
  attachUnavailable: "Native terminal bridge の接続または I/O 状態を確認できません。",
  exitCode: "終了コード",
  outputEnded: "Native 端末出力が終了しました。Issue の完了を示すものではありません。",
  unknownReadOnly: "状態不明のため読み取り専用です。再開や再送は行っていません。",
  unknownBlocksStart:
    "状態不明の native session があるため、新しい session の起動をブロックしています。",
  stop: "停止を要求",
  stopping: "停止要求中…",
  stopRequested: "Native session は停止要求中です。実際の終了状態はまだ確認できていません。",
  stopRequestError: "停止要求の結果を確認できません。session の状態を再取得してください。",
  reattach: "端末接続を更新",
} as const;
type TextKey = keyof typeof text;

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function isActiveSession(session: NativeTerminalSession): boolean {
  return session.state === "starting" || session.state === "running";
}

function hasNativeEnded(session: NativeTerminalSession): boolean {
  return (
    session.state === "closed" ||
    session.state === "cancelled" ||
    session.exitCode !== null ||
    (session.state !== "unknown" && session.endReceived && session.streamEof)
  );
}

function safePreviewArguments(arguments_: readonly string[]): string[] {
  const secretFlag =
    /^(?:--?|\/)?[^=]*(?:token|secret|password|passwd|credential|authorization|cookie|auth|api[-_]?key|access[-_]?key|private[-_]?key)[^=]*$/i;
  let redactNext = false;
  return arguments_.map((argument) => {
    if (redactNext) {
      redactNext = false;
      return "[REDACTED]";
    }
    const sanitizedUrl = argument.replace(/:\/\/[^/@\s]+@/g, "://[REDACTED]@");
    if (sanitizedUrl !== argument) return sanitizedUrl;
    const separator = argument.search(/[=:]/);
    const flag = separator < 0 ? argument : argument.slice(0, separator);
    const inlineCredential =
      /(?:token|secret|password|passwd|credential|authorization|cookie|auth|api[-_]?key|access[-_]?key|private[-_]?key)\s*[:=]/i.test(
        argument,
      );
    if (inlineCredential && !secretFlag.test(flag)) return "[REDACTED]";
    if (!secretFlag.test(flag)) return argument;
    if (separator >= 0) return `${flag}=[REDACTED]`;
    redactNext = true;
    return argument;
  });
}

export function NativeTerminalSessionPanel({ issueId }: { issueId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const statusQuery = useQuery({
    queryKey: ["native-terminal-status", issueId],
    queryFn: () => nativeTerminalStatus(issueId),
    retry: false,
  });
  const sessionsQuery = useQuery({
    queryKey: ["native-terminal-sessions", issueId],
    queryFn: () => nativeTerminalList(issueId),
    retry: false,
    refetchInterval: (query) => (query.state.data?.some(isActiveSession) === true ? 2000 : false),
  });
  const profiles = statusQuery.data?.profiles ?? [];
  const statusFingerprint = JSON.stringify(statusQuery.data ?? null);
  const observedStatusFingerprint = useRef(statusFingerprint);
  const statusGenerationRef = useRef(0);
  if (observedStatusFingerprint.current !== statusFingerprint) {
    observedStatusFingerprint.current = statusFingerprint;
    statusGenerationRef.current += 1;
  }
  const [profileChoice, setProfileChoice] = useState("");
  const profileId = profiles.some((profile) => profile.profileId === profileChoice)
    ? profileChoice
    : (profiles[0]?.profileId ?? "");
  const selectedProfile = profiles.find((profile) => profile.profileId === profileId);
  const [baseBranch, setBaseBranch] = useState("");
  const [prompt, setPrompt] = useState("");
  const [editGeneration, setEditGeneration] = useState(0);
  const editGenerationRef = useRef(0);
  const formKey = JSON.stringify([issueId, profileId, baseBranch, prompt, editGeneration]);
  const formKeyRef = useRef(formKey);
  formKeyRef.current = formKey;
  const [previewSnapshot, setPreviewSnapshot] = useState<PreviewSnapshot | null>(null);
  const [preparing, setPreparing] = useState(false);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [cancelPendingSessionId, setCancelPendingSessionId] = useState<string | null>(null);
  const [cancelError, setCancelError] = useState<string | null>(null);
  const [selectedSession, setSelectedSession] = useState<SelectedSession | null>(null);
  const [startedSession, setStartedSession] = useState<NativeTerminalSession | null>(null);
  const previewRequestId = useRef(0);
  const startRequestId = useRef(0);
  const previousIssueId = useRef(issueId);
  const issueIdRef = useRef(issueId);
  issueIdRef.current = issueId;
  const attemptedPreviewIds = useRef(new Set<string>());
  const attemptedCancelIds = useRef(new Set<string>());

  const issueSessions = useMemo(
    () => (sessionsQuery.data ?? []).filter((session) => session.issueId === issueId),
    [issueId, sessionsQuery.data],
  );
  const chosenSessionId = selectedSession?.issueId === issueId ? selectedSession.sessionId : null;
  const listedSession =
    issueSessions.find((session) => session.sessionId === chosenSessionId) ?? null;
  const fallbackStartedSession =
    startedSession?.issueId === issueId && startedSession.sessionId === chosenSessionId
      ? startedSession
      : null;
  const cancelAcknowledgement =
    fallbackStartedSession &&
    (fallbackStartedSession.cancelRequested || hasNativeEnded(fallbackStartedSession))
      ? fallbackStartedSession
      : null;
  const selectedStoredSession =
    listedSession && hasNativeEnded(listedSession)
      ? listedSession
      : (cancelAcknowledgement ?? listedSession ?? fallbackStartedSession);
  const terminalBridge = useNativeTerminalBridge(chosenSessionId, selectedStoredSession);
  const attachedSession =
    terminalBridge.session?.sessionId === chosenSessionId ? terminalBridge.session : null;
  const sessionMerge = mergeNativeTerminalSessionFacts(selectedStoredSession, attachedSession);
  const terminalSession = sessionMerge.session;
  const terminalFeedStatus =
    sessionMerge.protocolError !== null ||
    (terminalSession !== null && hasNativeTerminalInputBlocker(terminalSession))
      ? "unavailable"
      : terminalBridge.feedStatus;
  const promptBytes = new TextEncoder().encode(prompt).byteLength;
  const promptHasNul = prompt.includes("\u0000");
  const promptExceedsLimit = promptBytes > 64 * 1024;
  const currentPreview =
    previewSnapshot?.formKey === formKey &&
    previewSnapshot.editGeneration === editGenerationRef.current &&
    previewSnapshot.statusGeneration === statusGenerationRef.current
      ? previewSnapshot.preview
      : null;
  const previewIsStale = Boolean(
    previewSnapshot &&
      (previewSnapshot.formKey !== formKey ||
        previewSnapshot.editGeneration !== editGenerationRef.current ||
        previewSnapshot.statusGeneration !== statusGenerationRef.current),
  );
  const hasUnknownSession = issueSessions.some((session) => session.state === "unknown");
  const profileStillAllowed = Boolean(
    selectedProfile &&
      statusQuery.data?.profiles.some((profile) => profile.profileId === profileId) &&
      (baseBranch === ""
        ? selectedProfile.allowedBaseBranches.length > 0
        : selectedProfile.allowedBaseBranches.includes(baseBranch)),
  );
  const canPrepare = Boolean(
    statusQuery.data?.available === true &&
      profileStillAllowed &&
      !promptHasNul &&
      !promptExceedsLimit &&
      !preparing &&
      !starting,
  );
  const canStart = Boolean(
    statusQuery.isSuccess &&
      statusQuery.data?.available === true &&
      !statusQuery.isFetching &&
      profileStillAllowed &&
      currentPreview?.runtimeObserved === true &&
      currentPreview.profileId === profileId &&
      (baseBranch === ""
        ? selectedProfile?.allowedBaseBranches.includes(currentPreview.baseBranch)
        : currentPreview.baseBranch === baseBranch) &&
      previewSnapshot?.editGeneration === editGenerationRef.current &&
      previewSnapshot?.statusGeneration === statusGenerationRef.current &&
      previewSnapshot?.startAttempted === false &&
      !attemptedPreviewIds.current.has(currentPreview?.previewId ?? "") &&
      sessionsQuery.isSuccess &&
      !sessionsQuery.isFetching &&
      !hasUnknownSession &&
      !starting &&
      !preparing,
  );

  const markEdited = () => {
    editGenerationRef.current += 1;
    setEditGeneration(editGenerationRef.current);
    previewRequestId.current += 1;
    setPreparing(false);
  };

  useEffect(() => {
    if (previousIssueId.current === issueId) return;
    previousIssueId.current = issueId;
    previewRequestId.current += 1;
    startRequestId.current += 1;
    attemptedPreviewIds.current.clear();
    setProfileChoice("");
    setBaseBranch("");
    setPrompt("");
    editGenerationRef.current += 1;
    setEditGeneration(editGenerationRef.current);
    setPreviewSnapshot(null);
    setPreparing(false);
    setStarting(false);
    setError(null);
    setCancelPendingSessionId(null);
    setCancelError(null);
    setSelectedSession(null);
    setStartedSession(null);
  }, [issueId]);

  useEffect(() => {
    if (selectedSession?.issueId === issueId) return;
    const first = issueSessions[0];
    if (first) setSelectedSession({ issueId, sessionId: first.sessionId });
  }, [issueId, issueSessions, selectedSession]);

  const prepare = async () => {
    if (!canPrepare || !selectedProfile) return;
    const requestId = ++previewRequestId.current;
    const requestKey = formKey;
    const requestGeneration = editGenerationRef.current;
    const requestStatusGeneration = statusGenerationRef.current;
    setPreparing(true);
    setError(null);
    try {
      const result = await nativeTerminalPrepare({
        issueId,
        profileId,
        baseBranch,
        prompt: prompt.length === 0 ? null : prompt,
      });
      if (
        requestId !== previewRequestId.current ||
        formKeyRef.current !== requestKey ||
        editGenerationRef.current !== requestGeneration ||
        statusGenerationRef.current !== requestStatusGeneration
      ) {
        return;
      }
      setPreviewSnapshot({
        formKey: requestKey,
        editGeneration: requestGeneration,
        statusGeneration: requestStatusGeneration,
        preview: result,
        startAttempted: false,
      });
    } catch (reason) {
      if (
        requestId === previewRequestId.current &&
        formKeyRef.current === requestKey &&
        editGenerationRef.current === requestGeneration &&
        statusGenerationRef.current === requestStatusGeneration
      ) {
        setError(errorText(reason));
      }
    } finally {
      if (requestId === previewRequestId.current) setPreparing(false);
    }
  };

  const start = async () => {
    if (!canStart || !currentPreview || starting) return;
    const previewId = currentPreview.previewId;
    if (attemptedPreviewIds.current.has(previewId)) return;
    attemptedPreviewIds.current.add(previewId);
    const requestId = ++startRequestId.current;
    const requestKey = formKey;
    const previewDigest = currentPreview.previewDigest;
    setPreviewSnapshot((current) =>
      current?.preview.previewId === previewId ? { ...current, startAttempted: true } : current,
    );
    setStarting(true);
    setError(null);
    try {
      const session = await nativeTerminalStart({ previewId, previewDigest });
      if (
        session.issueId !== issueId ||
        session.sessionId !== currentPreview.sessionId ||
        session.previewId !== previewId ||
        session.profileId !== currentPreview.profileId
      ) {
        throw new Error("Native start returned a different prepared session.");
      }
      await queryClient.invalidateQueries({ queryKey: ["native-terminal-sessions", issueId] });
      if (requestId !== startRequestId.current || formKeyRef.current !== requestKey) return;
      setStartedSession(session);
      setSelectedSession({ issueId, sessionId: session.sessionId });
    } catch (reason) {
      if (requestId === startRequestId.current && formKeyRef.current === requestKey) {
        setError(errorText(reason));
      }
    } finally {
      if (requestId === startRequestId.current) setStarting(false);
    }
  };

  const refreshTerminal = async () => {
    await queryClient.invalidateQueries({ queryKey: ["native-terminal-sessions", issueId] });
    terminalBridge.reattach();
  };

  const cancel = async (sessionId: string) => {
    const target = terminalSession;
    const requestKey = JSON.stringify([issueId, sessionId]);
    if (
      !target ||
      target.sessionId !== sessionId ||
      hasNativeEnded(target) ||
      target.cancelRequested ||
      attemptedCancelIds.current.has(requestKey)
    ) {
      return;
    }
    attemptedCancelIds.current.add(requestKey);
    setCancelPendingSessionId(sessionId);
    setCancelError(null);
    try {
      const updated = await nativeTerminalCancel(sessionId);
      if (updated.sessionId !== sessionId || updated.issueId !== issueId) {
        throw new Error("Native cancel returned a different session owner.");
      }
      if (!updated.cancelRequested && !hasNativeEnded(updated)) {
        throw new Error("Native cancel did not confirm a stop request or a native end state.");
      }
      queryClient.setQueryData<NativeTerminalSession[]>(
        ["native-terminal-sessions", issueId],
        (sessions) =>
          sessions?.map((session) => (session.sessionId === sessionId ? updated : session)),
      );
      if (issueIdRef.current === issueId) setStartedSession(updated);
      await queryClient.invalidateQueries({ queryKey: ["native-terminal-sessions", issueId] });
    } catch (reason) {
      if (issueIdRef.current === issueId) setCancelError(errorText(reason));
      throw reason;
    } finally {
      if (issueIdRef.current === issueId) {
        setCancelPendingSessionId((current) => (current === sessionId ? null : current));
      }
    }
  };

  const japanese = (key: TextKey) => t(`nativeTerminal.${key}`, { defaultValue: text[key] });
  const sessionStateLabel = (session: NativeTerminalSession) => {
    const states = {
      starting: "起動中",
      running: "実行中",
      closed: "終了",
      cancelled: "取消済み",
      unknown: "状態不明（読み取り専用）",
    } as const;
    return t(`nativeTerminal.sessionState.${session.state}`, {
      defaultValue: states[session.state],
    });
  };

  return (
    <section
      aria-label={japanese("title")}
      className="settings-card"
      style={{ display: "grid", gap: 10, minWidth: 0 }}
    >
      <div>
        <div className="settings-card-title">{japanese("title")}</div>
        <p className="settings-card-desc">{japanese("description")}</p>
      </div>

      {statusQuery.isPending && <p role="status">{japanese("checking")}</p>}
      {statusQuery.isError && (
        <p role="alert" className="issues-error">
          {japanese("statusUnavailable")} {errorText(statusQuery.error)}
        </p>
      )}
      {statusQuery.data && !statusQuery.data.available && (
        <div role="status" className="settings-card-desc">
          {japanese("unavailable")}
          {statusQuery.data.blockers.map((blocker) => (
            <div key={blocker}>
              <code>{blocker}</code>
            </div>
          ))}
        </div>
      )}
      {statusQuery.data?.available === true && profiles.length === 0 && (
        <p role="status" className="settings-card-desc">
          {japanese("noProfiles")}
        </p>
      )}

      <label className="settings-row">
        <span className="settings-row-label">{japanese("profile")}</span>
        <select
          aria-label={japanese("profile")}
          value={profileId}
          disabled={profiles.length === 0 || !statusQuery.data?.available || starting}
          onChange={(event) => {
            markEdited();
            setProfileChoice(event.currentTarget.value);
          }}
        >
          <option value="">{japanese("chooseProfile")}</option>
          {profiles.map((profile) => (
            <option key={profile.profileId} value={profile.profileId}>
              {profile.label} · {profile.expectedCliVersion}
            </option>
          ))}
        </select>
      </label>
      <label className="settings-row">
        <span className="settings-row-label">{japanese("baseBranch")}</span>
        <select
          aria-label={japanese("baseBranch")}
          value={baseBranch}
          disabled={!selectedProfile || starting}
          onChange={(event) => {
            markEdited();
            setBaseBranch(event.currentTarget.value);
          }}
        >
          <option value="">{japanese("chooseBranch")}</option>
          {(selectedProfile?.allowedBaseBranches ?? []).map((branch) => (
            <option key={branch} value={branch}>
              {branch}
            </option>
          ))}
        </select>
      </label>
      {selectedProfile?.allowedBaseBranches.length === 0 && (
        <p role="status" className="settings-card-desc">
          {japanese("noAllowedBranches")}
        </p>
      )}
      <label className="settings-row">
        <span className="settings-row-label">{japanese("initialPrompt")}</span>
        <textarea
          aria-label={japanese("initialPrompt")}
          value={prompt}
          rows={3}
          disabled={starting}
          onChange={(event) => {
            markEdited();
            setPrompt(event.currentTarget.value);
          }}
        />
      </label>
      <p role="status" className="settings-card-desc">
        {japanese("promptByteLimit")}
      </p>
      {promptHasNul && (
        <p role="alert" className="issues-error">
          {japanese("promptNulRejected")}
        </p>
      )}
      {promptExceedsLimit && (
        <p role="alert" className="issues-error">
          {japanese("promptByteLimit")}
        </p>
      )}
      <button type="button" className="btn" disabled={!canPrepare} onClick={() => void prepare()}>
        {preparing ? japanese("preparing") : japanese("prepare")}
      </button>

      {error && (
        <p role="alert" className="issues-error">
          {error}
        </p>
      )}
      {currentPreview && (
        <section
          aria-label={japanese("preview")}
          className="coding-agent-run-preview"
          style={{ overflowWrap: "anywhere" }}
        >
          <h3>{japanese("preview")}</h3>
          <dl className="issues-definition">
            <dt>{japanese("previewId")}</dt>
            <dd>
              <code>{currentPreview.previewId}</code>
            </dd>
            <dt>{japanese("previewDigest")}</dt>
            <dd>
              <code>{currentPreview.previewDigest}</code>
            </dd>
            <dt>{japanese("repository")}</dt>
            <dd>
              {currentPreview.repository.host}/{currentPreview.repository.organization}/
              {currentPreview.repository.repository}
            </dd>
            <dt>{japanese("profile")}</dt>
            <dd>{currentPreview.profileId}</dd>
            <dt>{japanese("branch")}</dt>
            <dd>{currentPreview.baseBranch}</dd>
            <dt>{japanese("settingsRevision")}</dt>
            <dd>{currentPreview.expectedSettingsRevision}</dd>
            <dt>{japanese("issueRevision")}</dt>
            <dd>{currentPreview.expectedIssueRevision}</dd>
            <dt>{japanese("baseCommit")}</dt>
            <dd>
              <code>{currentPreview.baseCommit}</code>
            </dd>
            <dt>{japanese("cliVersion")}</dt>
            <dd>{currentPreview.expectedCliVersion}</dd>
            <dt>{japanese("imageDigest")}</dt>
            <dd>
              <code>{currentPreview.safeImageDigest}</code>
            </dd>
            <dt>{japanese("command")}</dt>
            <dd>
              <code style={{ display: "block" }}>
                {safePreviewArguments([currentPreview.command])[0]}
              </code>
              {safePreviewArguments(currentPreview.args).map((argument, index) => (
                // biome-ignore lint/suspicious/noArrayIndexKey: argv order and repeated arguments are meaningful.
                <code key={`${index}:${argument}`} style={{ display: "block" }}>
                  {argument}
                </code>
              ))}
            </dd>
            <dt>{japanese("workingDirectory")}</dt>
            <dd>
              <code>{currentPreview.workingDirectory}</code>
            </dd>
            <dt>{japanese("initialPrompt")}</dt>
            <dd>
              <pre style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>
                {currentPreview.initialPrompt}
              </pre>
            </dd>
          </dl>
          {!currentPreview.runtimeObserved && (
            <p role="status" className="issues-error">
              {japanese("runtimeNotObserved")}
            </p>
          )}
          {previewSnapshot?.startAttempted && (
            <p role="status" className="settings-card-desc">
              {japanese("startAttempted")}
            </p>
          )}
          {hasUnknownSession && (
            <p role="status" className="issues-error">
              {japanese("unknownBlocksStart")}
            </p>
          )}
          {currentPreview.runtimeObserved && (
            <button
              type="button"
              className="btn primary"
              disabled={!canStart}
              onClick={() => void start()}
            >
              {starting ? japanese("starting") : japanese("start")}
            </button>
          )}
        </section>
      )}
      {previewIsStale && (
        <p role="status" className="settings-card-desc">
          {japanese("previewStale")}
        </p>
      )}
      {previewSnapshot && !currentPreview && (
        <section className="coding-agent-run-preview" aria-label={japanese("previousPreview")}>
          <h3>{japanese("previousPreview")}</h3>
          <p className="settings-card-desc">{japanese("previewStale")}</p>
          <code>{previewSnapshot.preview.previewId}</code>
        </section>
      )}

      {sessionsQuery.isError && (
        <p role="alert" className="issues-error">
          {japanese("sessionsUnavailable")} {errorText(sessionsQuery.error)}
        </p>
      )}
      {issueSessions.length > 0 && (
        <label className="settings-row">
          <span className="settings-row-label">{japanese("savedSessions")}</span>
          <select
            aria-label={japanese("savedSessions")}
            value={chosenSessionId ?? ""}
            onChange={(event) =>
              setSelectedSession({ issueId, sessionId: event.currentTarget.value })
            }
          >
            {issueSessions.map((session) => (
              <option key={session.sessionId} value={session.sessionId}>
                {session.profileId} · {session.sessionId}
                {session.jobId ? ` · job ${session.jobId}` : ""}
              </option>
            ))}
          </select>
        </label>
      )}
      {terminalBridge.error && (
        <p role="alert" className="issues-error">
          {japanese("attachUnavailable")} {terminalBridge.error}
        </p>
      )}
      {sessionMerge.protocolError && (
        <p role="alert" className="issues-error">
          {sessionMerge.protocolError}
        </p>
      )}
      {terminalSession && (
        <section style={{ display: "grid", gap: 8 }}>
          <div role="status" className="settings-card-desc">
            {terminalSession.cancelRequested
              ? japanese("stopRequested")
              : sessionStateLabel(terminalSession)}
            {terminalSession.jobId ? ` · job ${terminalSession.jobId}` : ""}
            {terminalSession.exitCode !== null
              ? ` · ${japanese("exitCode")}: ${terminalSession.exitCode}`
              : ""}
          </div>
          {terminalSession.streamError && (
            <p role="status" className="settings-card-desc">
              {terminalSession.streamError}
            </p>
          )}
          {cancelError && (
            <p role="alert" className="issues-error">
              {japanese("stopRequestError")} {cancelError}
            </p>
          )}
          {!(terminalSession.state === "running" && terminalBridge.ownsSession) &&
            !hasNativeEnded(terminalSession) &&
            !terminalSession.cancelRequested && (
              <button
                type="button"
                className="btn"
                disabled={
                  cancelPendingSessionId === terminalSession.sessionId ||
                  attemptedCancelIds.current.has(
                    JSON.stringify([issueId, terminalSession.sessionId]),
                  )
                }
                onClick={() => void cancel(terminalSession.sessionId).catch(() => undefined)}
              >
                {cancelPendingSessionId === terminalSession.sessionId
                  ? japanese("stopping")
                  : japanese("stop")}
              </button>
            )}
          {(terminalBridge.session?.sessionId === terminalSession.sessionId ||
            terminalBridge.error !== null) &&
            terminalBridge.feedStatus !== "active" &&
            !hasNativeEnded(terminalSession) && (
              <button type="button" className="btn" onClick={() => void refreshTerminal()}>
                {japanese("reattach")}
              </button>
            )}
          <AgentTerminal
            bridge={terminalBridge.bridge}
            onCancelRequested={cancel}
            session={toAgentTerminalSession(
              terminalSession,
              terminalBridge.ownsSession,
              terminalFeedStatus,
            )}
          />
          {(terminalSession.endReceived || terminalSession.streamEof) && (
            <p role="status" className="settings-card-desc">
              {japanese("outputEnded")}
            </p>
          )}
          {terminalSession.state === "unknown" && (
            <p role="status" className="settings-card-desc">
              {japanese("unknownReadOnly")}
            </p>
          )}
        </section>
      )}
    </section>
  );
}
