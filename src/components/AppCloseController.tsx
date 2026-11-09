import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  acknowledgeCloseChoiceRequest,
  type BackgroundUnavailableReason,
  type CloseChoice,
  type CloseChoiceRequest,
  chooseCloseAction,
  quitApp,
  registerCloseChoiceUi,
  syncCloseChoiceLocale,
} from "@/api";
import { CloseChoiceDialog } from "@/components/CloseChoiceDialog";

interface CloseUiSession {
  active: boolean;
  ready: boolean;
  pendingRequests: CloseChoiceRequest[];
  acknowledgements: Set<number>;
}

function resolveUiLocale(locale: string | undefined): "ja" | "en" {
  return locale?.startsWith("ja") ? "ja" : "en";
}

export function AppCloseController() {
  const { i18n, t } = useTranslation();
  const locale = resolveUiLocale(i18n.resolvedLanguage ?? i18n.language);
  const localeRef = useRef(locale);
  localeRef.current = locale;
  const [request, setRequest] = useState<CloseChoiceRequest | null>(null);
  const [busy, setBusy] = useState(false);
  const [quitFailed, setQuitFailed] = useState(false);
  const [actionErrorKey, setActionErrorKey] = useState<string | null>(null);
  const activeRequestId = useRef<number | null>(null);
  const latestRequestIdSeen = useRef(0);
  const invalidatedRequestIdThrough = useRef(0);
  const acknowledgementGeneration = useRef(0);
  const requestRef = useRef<CloseChoiceRequest | null>(null);
  const activeUiSession = useRef<CloseUiSession | null>(null);
  const actionSequence = useRef(0);
  const activeAction = useRef<number | null>(null);
  const busyRef = useRef(false);

  const invalidateAction = useCallback(() => {
    actionSequence.current += 1;
    activeAction.current = null;
    busyRef.current = false;
    setBusy(false);
  }, []);

  const showAcknowledgedRequest = useCallback(
    async (next: CloseChoiceRequest, session: CloseUiSession) => {
      if (
        !session.active ||
        !session.ready ||
        activeRequestId.current === next.requestId ||
        session.acknowledgements.has(next.requestId) ||
        next.requestId <= invalidatedRequestIdThrough.current
      ) {
        return;
      }
      if (next.requestId < latestRequestIdSeen.current) return;
      latestRequestIdSeen.current = next.requestId;
      const generation = acknowledgementGeneration.current;
      session.acknowledgements.add(next.requestId);
      try {
        if (await acknowledgeCloseChoiceRequest(next.requestId)) {
          if (
            !session.active ||
            generation !== acknowledgementGeneration.current ||
            latestRequestIdSeen.current !== next.requestId ||
            next.requestId <= invalidatedRequestIdThrough.current
          ) {
            return;
          }
          if (requestRef.current?.requestId !== next.requestId) invalidateAction();
          activeRequestId.current = next.requestId;
          requestRef.current = next;
          setRequest(next);
          setActionErrorKey(null);
          setQuitFailed(false);
        }
      } catch {
        // Rust's acknowledgement timeout owns the native quit/cancel fallback.
      } finally {
        session.acknowledgements.delete(next.requestId);
      }
    },
    [invalidateAction],
  );

  useEffect(() => {
    const session: CloseUiSession = {
      active: true,
      ready: false,
      pendingRequests: [],
      acknowledgements: new Set(),
    };
    activeUiSession.current = session;
    let unlistenClose: (() => void) | undefined;
    let unlistenQuitFailed: (() => void) | undefined;

    const releaseListeners = () => {
      session.active = false;
      session.ready = false;
      session.pendingRequests = [];
      if (activeUiSession.current === session) activeUiSession.current = null;
      unlistenClose?.();
      unlistenClose = undefined;
      unlistenQuitFailed?.();
      unlistenQuitFailed = undefined;
    };

    void (async () => {
      try {
        unlistenClose = await listen<CloseChoiceRequest>(
          "window-lifecycle://close-requested",
          (event) => {
            if (!session.active) return;
            if (!session.ready) {
              session.pendingRequests.push(event.payload);
              return;
            }
            void showAcknowledgedRequest(event.payload, session);
          },
        );
        if (!session.active) {
          releaseListeners();
          return;
        }
        unlistenQuitFailed = await listen<{ nativeFallback: boolean }>(
          "window-lifecycle://quit-failed",
          (event) => {
            if (!session.active) return;
            const invalidatedIds = [
              latestRequestIdSeen.current,
              activeRequestId.current ?? 0,
              requestRef.current?.requestId ?? 0,
              ...session.pendingRequests.map((pending) => pending.requestId),
              ...session.acknowledgements,
            ];
            invalidatedRequestIdThrough.current = Math.max(
              invalidatedRequestIdThrough.current,
              ...invalidatedIds,
            );
            acknowledgementGeneration.current += 1;
            session.acknowledgements.clear();
            session.pendingRequests = [];
            requestRef.current = null;
            activeRequestId.current = null;
            invalidateAction();
            setRequest(null);
            setActionErrorKey(null);
            setQuitFailed(!event.payload.nativeFallback);
          },
        );
        if (!session.active) {
          releaseListeners();
          return;
        }
        const bootstrapGeneration = acknowledgementGeneration.current;
        const bootstrap = await registerCloseChoiceUi(localeRef.current);
        if (!session.active) return;
        session.ready = true;
        const pendingRequests = session.pendingRequests.splice(0);
        if (bootstrapGeneration === acknowledgementGeneration.current && bootstrap.pendingRequest) {
          pendingRequests.unshift(bootstrap.pendingRequest);
        }
        for (const pending of pendingRequests) {
          void showAcknowledgedRequest(pending, session);
        }
      } catch {
        // Registration failure is handled by Rust's timed native fallback.
        releaseListeners();
      }
    })();

    return () => {
      releaseListeners();
      actionSequence.current += 1;
      activeAction.current = null;
      busyRef.current = false;
    };
    // The registration locale is refreshed separately to avoid rebuilding listeners.
  }, [invalidateAction, showAcknowledgedRequest]);

  useEffect(() => {
    void syncCloseChoiceLocale(locale).catch(() => {
      // Locale synchronization is best-effort; fallback defaults to bilingual.
    });
  }, [locale]);

  const clearRequest = (actionId: number) => {
    if (activeAction.current !== actionId) return;
    activeAction.current = null;
    actionSequence.current += 1;
    activeRequestId.current = null;
    requestRef.current = null;
    setRequest(null);
    busyRef.current = false;
    setBusy(false);
    setActionErrorKey(null);
  };

  const runAction = async (choice: CloseChoice) => {
    if (busyRef.current) return;
    const current = requestRef.current;
    const actionId = ++actionSequence.current;
    activeAction.current = actionId;
    const isCurrentAction = () =>
      activeAction.current === actionId && requestRef.current?.requestId === current?.requestId;
    busyRef.current = true;
    setBusy(true);
    setActionErrorKey(null);
    setQuitFailed(false);
    if (!current) {
      if (choice === "quit") {
        try {
          await quitApp();
        } catch {
          if (!isCurrentAction()) return;
          activeAction.current = null;
          busyRef.current = false;
          setBusy(false);
          setQuitFailed(true);
        }
      } else {
        if (!isCurrentAction()) return;
        activeAction.current = null;
        busyRef.current = false;
        setBusy(false);
        setQuitFailed(false);
      }
      return;
    }

    try {
      const result = await chooseCloseAction(current.requestId, choice);
      if (!isCurrentAction()) return;
      if (result === "accepted") {
        if (choice !== "quit") clearRequest(actionId);
        return;
      }
      if (result === "backgroundUnavailable" || result === "hideFailed") {
        const reason: BackgroundUnavailableReason =
          result === "hideFailed" ? "hide_failed" : "unavailable";
        const updated = {
          ...current,
          backgroundAvailable: false,
          backgroundUnavailableReason: reason,
        };
        requestRef.current = updated;
        setRequest(updated);
        activeAction.current = null;
        busyRef.current = false;
        setBusy(false);
        setActionErrorKey(
          result === "hideFailed"
            ? "app.closeChoice.backgroundHideFailed"
            : `app.closeChoice.backgroundUnavailable.${reason}`,
        );
        return;
      }
      clearRequest(actionId);
    } catch {
      if (!isCurrentAction()) return;
      activeAction.current = null;
      busyRef.current = false;
      setBusy(false);
      if (choice === "quit") {
        setQuitFailed(true);
      } else {
        setActionErrorKey("app.closeChoice.actionFailed");
      }
    }
  };

  const dismiss = () => {
    if (busyRef.current) return;
    if (requestRef.current) {
      void runAction("cancel");
    } else {
      setQuitFailed(false);
      setActionErrorKey(null);
    }
  };

  return (
    <CloseChoiceDialog
      open={request !== null || quitFailed}
      backgroundAvailable={request?.backgroundAvailable ?? false}
      backgroundUnavailableReason={
        request?.backgroundUnavailableReason ?? (quitFailed ? "unavailable" : null)
      }
      busy={busy}
      error={
        actionErrorKey ? t(actionErrorKey) : quitFailed ? t("app.closeChoice.quitFailed") : null
      }
      onQuit={() => void runAction("quit")}
      onBackground={() => void runAction("background")}
      onCancel={dismiss}
    />
  );
}
