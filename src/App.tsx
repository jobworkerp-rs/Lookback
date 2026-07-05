import { useQuery, useQueryClient } from "@tanstack/react-query";
import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  countSummaries,
  getConnectionConfig,
  getPersonality,
  getSetupStatus,
  setOutputLanguage,
} from "@/api";
import { BootError } from "@/components/BootError";
import { BootScreen } from "@/components/BootScreen";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { ErrorBoundary } from "@/components/ErrorBoundary";
import { ImportDialog } from "@/components/ImportDialog";
import { ImportToast } from "@/components/ImportToast";
import { SetupWizard } from "@/components/SetupWizard";
import { type Route, Sidebar } from "@/components/Sidebar";
import { VectorDegradedBanner } from "@/components/VectorDegradedBanner";
import { useGeneratedRefresh } from "@/hooks/useGeneratedRefresh";
import { useImportProgress } from "@/hooks/useImportProgress";
import { useImportRefresh } from "@/hooks/useImportRefresh";
import { useLocale } from "@/hooks/useLocale";
import { useRagChat } from "@/hooks/useRagChat";
import { useReflectionProgress } from "@/hooks/useReflectionProgress";
import { useSettingsDirty } from "@/hooks/useSettingsDirty";
import { isVectorDegraded, useSidecarStatus } from "@/hooks/useSidecarStatus";
import { usePersonalityProgress, useSummaryProgress } from "@/hooks/useStepStreamProgress";
import { useTheme } from "@/hooks/useTheme";
import { PERSONALITY_QUERY_KEY } from "@/lib/queryKeys";
import { Chat } from "@/pages/Chat";
import { PeriodicTasks } from "@/pages/PeriodicTasks";
import { Personality } from "@/pages/Personality";
import { Reflections } from "@/pages/Reflections";
import { type EmbeddingFocus, Settings } from "@/pages/Settings";
import { Summaries, type SummariesFocus } from "@/pages/Summaries";
import { Threads } from "@/pages/Threads";

export function App() {
  const { t } = useTranslation();
  const sidecar = useSidecarStatus();
  const theme = useTheme();
  const locale = useLocale();
  const settingsDirty = useSettingsDirty();
  const [route, setRoute] = useState<Route>("threads");
  // Pending navigation parked while the leave-guard confirm is open.
  const [pendingRoute, setPendingRoute] = useState<Route | null>(null);
  const [importOpen, setImportOpen] = useState(false);
  const importProgress = useImportProgress();
  // Analysis progress is owned here, not inside each page, so a generation
  // running in the background keeps streaming and its progress survives when
  // the user switches tabs (the pages mount/unmount on route change).
  const summaryProgress = useSummaryProgress();
  const personalityProgress = usePersonalityProgress();
  const reflectionProgress = useReflectionProgress();
  // Same reason as the progress hooks above: the Chat page unmounts on
  // route change, so its in-flight conversation (turns + the
  // `chat://step` event listener) would die with it. Hoisting the hook
  // here keeps the listener attached for the whole session and lets the
  // user tab away to inspect a source thread without losing context.
  const rag = useRagChat();
  // Deep-link seed for the Summaries tab — set by `Chat`'s period_summary
  // source pill so the user lands on the cited calendar entry instead of
  // the top-level tab. `Summaries` clears it via `onFocusConsumed`.
  const [summariesFocus, setSummariesFocus] = useState<SummariesFocus | null>(null);
  // Deep-link seed for the Settings tab's embedding card — set by the
  // vector-degraded banner's CTA so the user lands directly on the embedding
  // model settings (advanced view + scrolled into view). `Settings` clears it
  // via `onEmbeddingFocusConsumed`.
  const [embeddingFocus, setEmbeddingFocus] = useState<EmbeddingFocus | null>(null);
  const queryClient = useQueryClient();
  useGeneratedRefresh(queryClient);
  useImportRefresh(queryClient, importProgress.snapshot);
  const setupStatus = useQuery({
    queryKey: ["setup-status"],
    queryFn: getSetupStatus,
    enabled: sidecar.phase === "ready",
    staleTime: Number.POSITIVE_INFINITY,
  });
  const connectionConfig = useQuery({
    queryKey: ["connection-config"],
    queryFn: getConnectionConfig,
    enabled: sidecar.phase === "ready",
  });
  // Vector-store degraded diagnostics (null unless the sidecar restarted with
  // vectors disabled). It only gates local connection mode; remote targets own
  // their vector store state independently from the local sidecar warning.
  const connectionMode = connectionConfig.data?.mode ?? null;
  const degraded = isVectorDegraded(sidecar, connectionMode);

  // Mirror the resolved UI locale into the backend's `app-settings.json` so
  // headless generation (conductor periodic runs, which never touch the
  // frontend) produces output in the language the UI is set to. The UI locale
  // itself stays in localStorage (`useLocale` is deliberately Tauri-free so a
  // data purge never touches it); this is the one Tauri-aware mirror. Gated on
  // `ready` so the command reaches a live backend. Best-effort: a failure just
  // means periodic falls back to the persisted/default language.
  useEffect(() => {
    if (sidecar.phase !== "ready") return;
    void setOutputLanguage(locale.resolved).catch(() => {
      /* non-fatal: periodic falls back to the previous persisted value */
    });
  }, [sidecar.phase, locale.resolved]);

  // Leave-guard: intercept a navigation away from a dirty Settings tab so
  // unsaved (restart-bearing) changes aren't silently dropped on tab
  // switch. All navigation entry points route through this instead of the
  // raw setRoute. A move INTO settings, or within settings, is never
  // guarded.
  const guardedSetRoute = (next: Route) => {
    if (route === "settings" && next !== "settings" && settingsDirty.dirty) {
      setPendingRoute(next);
    } else {
      setRoute(next);
    }
  };

  // Sidebar's thread count piggybacks on the Personality query —
  // `get_personality` already returns `thread_count` (derived server-side
  // because `ThreadService.Count` has no user_id filter). Sharing the
  // query key dedupes with the Personality tab.
  const personality = useQuery({
    queryKey: PERSONALITY_QUERY_KEY,
    queryFn: () => getPersonality({ user_id: 1 }),
    enabled: sidecar.phase === "ready",
    refetchInterval: 30_000,
  });

  const summaryCount = useQuery({
    queryKey: ["count-summaries"],
    queryFn: () => countSummaries(),
    enabled: sidecar.phase === "ready",
    refetchInterval: 15000,
  });

  const importBin = useQuery({
    queryKey: ["resolve-memories-import-bin"],
    queryFn: () => invoke<string>("resolve_memories_import_bin"),
    enabled: sidecar.phase === "ready",
    retry: false,
  });

  if (sidecar.phase === "starting") {
    return <BootScreen title="Lookback" detail={t("boot.startingSidecar")} />;
  }

  // Boot-time recovery surface. A `sidecar://error` (lancedb schema
  // mismatch, RDB init failure, generic raw rollback message, …) lands
  // in `sidecar.failure` and BootError renders the per-code recovery
  // actions full-screen until the user takes one.
  if (sidecar.phase === "error" && sidecar.failure) {
    return <BootError failure={sidecar.failure} />;
  }

  // The first-run gate, evaluated once the sidecar is up: probe state → its
  // error fallback → the wizard. Grouped under one phase check so the
  // precedence reads as a single decision.
  if (sidecar.phase === "ready") {
    if (setupStatus.isPending) {
      return <BootScreen title="Lookback" detail={t("boot.checkingSetup")} />;
    }
    if (setupStatus.isError) {
      return (
        <BootScreen
          error
          detail={t("boot.setupCheckFailed")}
          action={
            <button type="button" className="btn" onClick={() => void setupStatus.refetch()}>
              {t("boot.retry")}
            </button>
          }
        />
      );
    }
    if (setupStatus.data?.required) {
      return (
        <SetupWizard
          resumeApply={setupStatus.data.resume_apply}
          currentDataRoot={setupStatus.data.current_data_root}
          defaultDataRoot={setupStatus.data.default_data_root}
          onComplete={() => {
            queryClient.setQueryData(["setup-status"], {
              ...setupStatus.data,
              required: false,
              resume_apply: false,
            });
          }}
        />
      );
    }
  }

  return (
    <div className="app">
      <Sidebar
        current={route}
        onChange={guardedSetRoute}
        threadCount={personality.data?.thread_count ?? null}
        threadCountTruncated={personality.data?.thread_count_truncated ?? false}
        summaryCount={summaryCount.data ?? null}
        sidecar={sidecar}
        theme={theme}
        locale={locale}
      />
      <main className="main">
        {/* Non-fatal degraded banner: the sidecar came up but the local
            vector store is disabled (dimension mismatch). Shown above every
            tab with a shortcut into the embedding settings card. */}
        {degraded && (
          <VectorDegradedBanner
            info={degraded}
            onOpenEmbeddingSettings={() => {
              setEmbeddingFocus({ nonce: Date.now() });
              guardedSetRoute("settings");
            }}
          />
        )}
        {/* Keyed by route so a crash in one tab shows a fallback while the
            sidebar stays usable, and switching tabs remounts the boundary
            (clearing the error). */}
        <ErrorBoundary key={route}>
          {route === "threads" && (
            <Threads
              onOpenImport={() => setImportOpen(true)}
              sidecar={sidecar}
              connectionMode={connectionMode}
            />
          )}
          {route === "summaries" && (
            <Summaries
              summaryProgress={summaryProgress}
              sidecar={sidecar}
              connectionMode={connectionMode}
              focus={summariesFocus}
              onFocusConsumed={() => setSummariesFocus(null)}
            />
          )}
          {route === "reflections" && (
            <Reflections
              reflectionProgress={reflectionProgress}
              sidecar={sidecar}
              connectionMode={connectionMode}
            />
          )}
          {route === "personality" && (
            <Personality personalityProgress={personalityProgress} onNavigate={setRoute} />
          )}
          {route === "chat" && (
            <Chat
              onNavigate={setRoute}
              rag={rag}
              onNavigateSummariesFocus={(focus) => {
                setSummariesFocus(focus);
                setRoute("summaries");
              }}
            />
          )}
          {route === "periodic" && <PeriodicTasks />}
          {route === "settings" && (
            <Settings
              dirty={settingsDirty}
              embeddingFocus={embeddingFocus}
              onEmbeddingFocusConsumed={() => setEmbeddingFocus(null)}
            />
          )}
        </ErrorBoundary>
      </main>

      {pendingRoute && (
        <ConfirmDialog
          title={t("app.leaveGuard.title")}
          message={t("app.leaveGuard.message")}
          confirmLabel={t("app.leaveGuard.confirm")}
          onConfirm={() => {
            settingsDirty.setDirty(false);
            setRoute(pendingRoute);
            setPendingRoute(null);
          }}
          onCancel={() => setPendingRoute(null)}
        />
      )}

      <ImportDialog
        open={importOpen}
        onClose={() => setImportOpen(false)}
        onStarted={(jobId) => {
          importProgress.reset(jobId);
          void queryClient.invalidateQueries({ queryKey: ["threads"] });
          void queryClient.invalidateQueries({ queryKey: ["summaries"] });
        }}
        memoriesImportBin={importBin.data ?? ""}
        resolveError={importBin.error ? String(importBin.error) : null}
        sidecar={sidecar}
      />

      {importProgress.snapshot && (
        <ImportToast
          snapshot={importProgress.snapshot}
          onClose={importProgress.clear}
          onCancel={importProgress.cancel}
        />
      )}
    </div>
  );
}
