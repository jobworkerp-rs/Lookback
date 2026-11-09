import { useQuery, useQueryClient } from "@tanstack/react-query";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  getAppCapabilities,
  getAppSettings,
  getConnectionConfig,
  getSetupStatus,
  setOutputLanguage,
} from "@/api";
import { BootError } from "@/components/BootError";
import { BootScreen } from "@/components/BootScreen";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { EmbeddingSpaceStatus } from "@/components/EmbeddingSpaceStatus";
import { ErrorBoundary } from "@/components/ErrorBoundary";
import { ImportDialog } from "@/components/ImportDialog";
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
import { useSidebarCounts } from "@/hooks/useSidebarCounts";
import { isVectorDegraded, useSidecarStatus } from "@/hooks/useSidecarStatus";
import { useSummaryGenerationProgress } from "@/hooks/useStagedProgress";
import { usePersonalityProgress, useSummaryProgress } from "@/hooks/useStepStreamProgress";
import { useTheme } from "@/hooks/useTheme";
import { TimezoneContext } from "@/hooks/useTimezone";
import { connectionSignature } from "@/lib/connectionConfig";
import { Chat } from "@/pages/Chat";
import { Issues } from "@/pages/Issues";
import { PeriodicTasks } from "@/pages/PeriodicTasks";
import { Personality } from "@/pages/Personality";
import { Reflections } from "@/pages/Reflections";
import { type EmbeddingFocus, Settings } from "@/pages/Settings";
import { Summaries, type SummariesFocus } from "@/pages/Summaries";
import { Threads } from "@/pages/Threads";
import type { EmbeddingMigrationFailedEvent } from "@/types/api";

export function App() {
  const { t } = useTranslation();
  const sidecar = useSidecarStatus();
  const theme = useTheme();
  const locale = useLocale();
  const settingsDirty = useSettingsDirty();
  // App-wide display timezone, read once here and shared via TimezoneContext so
  // every leaf timestamp view can render in the configured zone without each
  // one needing its own `["app-settings"]` query. `effective_timezone` is the
  // backend-resolved zone (explicit selection → env/OS fallback); the Settings
  // save invalidates `["app-settings"]`, so a change re-renders all timestamps.
  const appSettings = useQuery({ queryKey: ["app-settings"], queryFn: getAppSettings });
  const appCapabilities = useQuery({
    queryKey: ["app-capabilities"],
    queryFn: getAppCapabilities,
    staleTime: Number.POSITIVE_INFINITY,
  });
  const experimentalIssuesEnabled = appCapabilities.data?.experimental_issues_enabled === true;
  const issueTrackingConfigurable = appCapabilities.data?.issue_tracking_configurable === true;
  const timezone = appSettings.data?.effective_timezone;
  const [route, setRoute] = useState<Route>("threads");
  // Pending navigation parked while the leave-guard confirm is open.
  const [pendingRoute, setPendingRoute] = useState<Route | null>(null);
  const [importOpen, setImportOpen] = useState(false);
  const importProgress = useImportProgress();
  // Analysis progress is owned here, not inside each page, so a generation
  // running in the background keeps streaming and its progress survives when
  // the user switches tabs (the pages mount/unmount on route change).
  const summaryProgress = useSummaryProgress();
  const summaryGenerationProgress = useSummaryGenerationProgress();
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
  const [threadGroupFocus, setThreadGroupFocus] = useState<string | null>(null);
  // Deep-link seed for the Settings tab's embedding card — set by the
  // vector-degraded banner's CTA so the user lands directly on the embedding
  // model settings (advanced view + scrolled into view). `Settings` clears it
  // via `onEmbeddingFocusConsumed`.
  const [embeddingFocus, setEmbeddingFocus] = useState<EmbeddingFocus | null>(null);
  const queryClient = useQueryClient();
  const [sidecarReadyEpoch, setSidecarReadyEpoch] = useState(0);
  const [embeddingMigrationFailure, setEmbeddingMigrationFailure] =
    useState<EmbeddingMigrationFailedEvent | null>(null);
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
  const lastConnectionSignature = useRef<string | null>(null);
  const targetSignature = connectionConfig.data ? connectionSignature(connectionConfig.data) : null;
  useEffect(() => {
    if (!targetSignature) return;
    if (
      lastConnectionSignature.current !== null &&
      lastConnectionSignature.current !== targetSignature
    ) {
      void queryClient.invalidateQueries({ queryKey: ["embedding-space"] });
    }
    lastConnectionSignature.current = targetSignature;
  }, [targetSignature, queryClient]);

  useEffect(() => {
    let disposed = false;
    let unlistenReady: (() => void) | undefined;
    let unlistenFailure: (() => void) | undefined;
    void listen("sidecar://ready", () => {
      setSidecarReadyEpoch((epoch) => epoch + 1);
      void queryClient.invalidateQueries({ queryKey: ["embedding-space"] });
      void queryClient.invalidateQueries({ queryKey: ["embedding-migration-inspect"] });
      setEmbeddingMigrationFailure(null);
    }).then((unlisten) => {
      if (disposed) unlisten();
      else unlistenReady = unlisten;
    });
    void listen<EmbeddingMigrationFailedEvent>("embedding_migration_failed", (event) => {
      setEmbeddingMigrationFailure(event.payload);
      // The event is a warning only. The migration panel always inspects the
      // actual service state before making any operation available.
      void queryClient.invalidateQueries({ queryKey: ["embedding-space"] });
      void queryClient.invalidateQueries({ queryKey: ["embedding-migration-inspect"] });
    }).then((unlisten) => {
      if (disposed) unlisten();
      else unlistenFailure = unlisten;
    });
    return () => {
      disposed = true;
      unlistenReady?.();
      unlistenFailure?.();
    };
  }, [queryClient]);
  // Vector-store degraded diagnostics (null unless the sidecar restarted with
  // vectors disabled). It only gates a local memories; a remote one owns its
  // vector store state independently from the local sidecar warning.
  const memoriesMode = connectionConfig.data?.memories.mode ?? null;
  const degraded = isVectorDegraded(sidecar, memoriesMode);

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

  const sidebarCounts = useSidebarCounts(sidecar.phase === "ready");

  const importBin = useQuery({
    queryKey: ["resolve-memories-import-bin"],
    queryFn: () => invoke<string>("resolve_memories_import_bin"),
    enabled: sidecar.phase === "ready",
    retry: false,
  });

  if (sidecar.phase === "starting") {
    return (
      <BootScreen
        title="Lookback"
        detail={
          sidecar.databaseMigrationInProgress
            ? t("boot.databaseMigrationInProgress")
            : t("boot.startingSidecar")
        }
      />
    );
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
    <TimezoneContext.Provider value={timezone}>
      <div className="app">
        <Sidebar
          current={route}
          onChange={guardedSetRoute}
          threadCount={sidebarCounts.threadCount}
          threadCountTruncated={sidebarCounts.threadCountTruncated}
          threadGroupCount={sidebarCounts.threadGroupCount}
          summaryCount={sidebarCounts.summaryCount}
          sidecar={sidecar}
          theme={theme}
          locale={locale}
          experimentalIssuesEnabled={experimentalIssuesEnabled}
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
          {(["threads", "summaries", "reflections", "chat"] as const).includes(
            route as "threads" | "summaries" | "reflections" | "chat",
          ) && (
            <EmbeddingSpaceStatus
              readyEpoch={sidecarReadyEpoch}
              failureEvent={embeddingMigrationFailure}
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
                memoriesMode={memoriesMode}
                importSnapshot={importProgress.snapshot}
                importBusy={importProgress.busy}
                onCancelImport={importProgress.cancel}
                onClearImportProgress={importProgress.clear}
                initialGroupId={threadGroupFocus}
                onInitialGroupConsumed={() => setThreadGroupFocus(null)}
                summaryProgress={summaryProgress}
              />
            )}
            {route === "summaries" && (
              <Summaries
                summaryGenerationProgress={summaryGenerationProgress}
                sidecar={sidecar}
                memoriesMode={memoriesMode}
                focus={summariesFocus}
                onFocusConsumed={() => setSummariesFocus(null)}
                onNavigateThreadGroup={(groupId) => {
                  setThreadGroupFocus(groupId);
                  setRoute("threads");
                }}
              />
            )}
            {route === "reflections" && (
              <Reflections
                reflectionProgress={reflectionProgress}
                sidecar={sidecar}
                memoriesMode={memoriesMode}
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
                onNavigateThreadGroup={(groupId) => {
                  setThreadGroupFocus(groupId);
                  setRoute("threads");
                }}
              />
            )}
            {route === "periodic" && <PeriodicTasks />}
            {route === "issues" && experimentalIssuesEnabled && <Issues />}
            {route === "settings" && (
              <Settings
                dirty={settingsDirty}
                embeddingFocus={embeddingFocus}
                onEmbeddingFocusConsumed={() => setEmbeddingFocus(null)}
                experimentalIssuesEnabled={experimentalIssuesEnabled}
                issueTrackingConfigurable={issueTrackingConfigurable}
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
      </div>
    </TimezoneContext.Provider>
  );
}
