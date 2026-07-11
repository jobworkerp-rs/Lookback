import { useQuery, useQueryClient } from "@tanstack/react-query";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  applySettings,
  createDataRoot,
  getAppSettings,
  getBackgroundJobQueueStatus,
  getConnectionConfig,
  getEmbeddingSettings,
  getLlmSettings,
  getMcpSettings,
  getMemoryEmbeddingStats,
  getModelStatus,
  getSettings,
  listEmbeddingPresets,
  listLlmPresets,
  purgeAllData,
  readSidecarLog,
  redispatchMemoryEmbeddings,
  retryModelSetup,
  setConnectionConfig,
  setDataRoot,
  testConnectionConfig,
  validateDataRoot,
} from "@/api";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { Modal } from "@/components/Modal";
import { Toolbar } from "@/components/Toolbar";
import type { SettingsDirtyControl } from "@/hooks/useSettingsDirty";
import type {
  BackgroundTaskKind,
  ConnectionMode,
  DataRootValidation,
  EmbeddingPreset,
  HfHomeMode,
  KvCacheType,
  LlmMode,
  LlmPreset,
  LogSource,
  LogStream,
  ModelState,
  ModelStatus,
  RedispatchEmbeddingsResult,
  SetEmbeddingSettingsRequest,
  SetHfHomeRequest,
  SetLlmSettingsRequest,
  SetMcpSettingsRequest,
  SetTimezoneRequest,
} from "@/types/api";
import { CUSTOM_EMBEDDING_PRESET_ID, CUSTOM_LLM_PRESET_ID } from "@/types/api";
import { SettingsSaveBar } from "./SettingsSaveBar";
import { TimezoneCard } from "./TimezoneCard";

/** One-shot deep-link seed to open the embedding model card. `nonce` makes
 * every request distinct so re-triggering (e.g. clicking the banner CTA
 * twice) re-runs the focus effect. */
export interface EmbeddingFocus {
  nonce: number;
}

export function Settings({
  dirty,
  embeddingFocus,
  onEmbeddingFocusConsumed,
}: {
  dirty?: SettingsDirtyControl;
  embeddingFocus?: EmbeddingFocus | null;
  onEmbeddingFocusConsumed?: () => void;
} = {}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { data, refetch } = useQuery({
    queryKey: ["settings"],
    queryFn: getSettings,
  });
  const connectionConfig = useQuery({
    queryKey: ["connection-config"],
    queryFn: getConnectionConfig,
  });
  const modelStatus = useQuery({
    queryKey: ["model-status"],
    queryFn: getModelStatus,
    // Poll while EITHER model is still downloading; stop once both settle.
    refetchInterval: (q) => {
      const d = q.state.data;
      const preparing = d?.llm.state === "preparing" || d?.embedding.state === "preparing";
      return preparing ? 3000 : false;
    },
  });
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [retrying, setRetrying] = useState(false);

  // ── Unified-save aggregation ──
  // Each restart-bearing card reports its payload (or null when clean) via
  // onDirtyChange; the bottom save bar commits all three in one restart.
  const [llmPayload, setLlmPayload] = useState<SetLlmSettingsRequest | null>(null);
  const [embPayload, setEmbPayload] = useState<SetEmbeddingSettingsRequest | null>(null);
  const [hfPayload, setHfPayload] = useState<SetHfHomeRequest | null>(null);
  const [tzPayload, setTzPayload] = useState<SetTimezoneRequest | null>(null);
  const [mcpPayload, setMcpPayload] = useState<SetMcpSettingsRequest | null>(null);
  // "edited" tracks whether each card's form differs from the persisted
  // value regardless of validity. The leave-guard keys off this so an
  // invalid-but-edited form still blocks an unconfirmed navigation; the
  // save bar keys off the payload (an invalid form is not saveable).
  const [llmEdited, setLlmEdited] = useState(false);
  const [embEdited, setEmbEdited] = useState(false);
  const [hfEdited, setHfEdited] = useState(false);
  const [tzEdited, setTzEdited] = useState(false);
  const [mcpEdited, setMcpEdited] = useState(false);
  // Connection / Data location are self-contained cards (their own Save
  // button, NOT part of the unified save bar), but their in-progress edits
  // must still arm the leave-guards — otherwise switching back to the basic
  // view or to another tab unmounts them and drops the input silently. They
  // report only an `edited` flag (no save payload) for that purpose.
  const [connEdited, setConnEdited] = useState(false);
  const [dataRootEdited, setDataRootEdited] = useState(false);
  // Whether the pending embedding change will actually reset the vectordb
  // (model id or vector dimension changes) — as opposed to a dtype / max-seq
  // / tokenizer / multimodal-only tweak, which the backend's
  // `needs_vectordb_reset` leaves false. Drives the destructive warning so it
  // only fires when the index really will be evacuated / deleted.
  const [embResetsVectordb, setEmbResetsVectordb] = useState(false);
  const [resetSignal, setResetSignal] = useState(0);

  // Adapters: each card reports (payload, edited); fan them into the two
  // parent state slots. Stable identities so the cards' report effects
  // don't loop.
  const reportLlm = useCallback((p: SetLlmSettingsRequest | null, edited: boolean) => {
    setLlmPayload(p);
    setLlmEdited(edited);
  }, []);
  const reportEmb = useCallback((p: SetEmbeddingSettingsRequest | null, edited: boolean) => {
    setEmbPayload(p);
    setEmbEdited(edited);
  }, []);
  const reportEmbResets = useCallback((resets: boolean) => setEmbResetsVectordb(resets), []);
  const reportHf = useCallback((p: SetHfHomeRequest | null, edited: boolean) => {
    setHfPayload(p);
    setHfEdited(edited);
  }, []);
  const reportTz = useCallback((p: SetTimezoneRequest | null, edited: boolean) => {
    setTzPayload(p);
    setTzEdited(edited);
  }, []);
  const reportMcp = useCallback((p: SetMcpSettingsRequest | null, edited: boolean) => {
    setMcpPayload(p);
    setMcpEdited(edited);
  }, []);
  // Stable identities so the self-contained cards' report effects don't loop.
  const reportConnEdited = useCallback((edited: boolean) => setConnEdited(edited), []);
  const reportDataRootEdited = useCallback((edited: boolean) => setDataRootEdited(edited), []);
  const [applyBusy, setApplyBusy] = useState(false);
  const [applyError, setApplyError] = useState<string | null>(null);
  const [backupPath, setBackupPath] = useState<string | null>(null);

  // ── Sub-view ──
  // The advanced view holds the low-frequency, high-impact settings
  // (Connection / Embedding model / Data location / Destructive) so the
  // basic view stays uncluttered. Switching is in-component state, not a
  // route — the Sidebar keeps a single "設定" entry.
  const [view, setView] = useState<"basic" | "advanced">("basic");
  const [pendingView, setPendingView] = useState<"basic" | "advanced" | null>(null);
  // Reset the scroll position on a view switch so the new view starts at
  // the top (the advanced view leads with a notice the user must read);
  // otherwise the scroll offset from the previous view carries over and
  // lands mid-content.
  const contentRef = useRef<HTMLDivElement>(null);
  // biome-ignore lint/correctness/useExhaustiveDependencies: `view` is the trigger, not read in the body
  useEffect(() => {
    const el = contentRef.current;
    if (!el) return;
    // `scrollTo` is absent in jsdom; `scrollTop = 0` works everywhere.
    el.scrollTop = 0;
  }, [view]);

  // Deep-link into the embedding model card (banner CTA): switch to the
  // advanced view via `requestView` (so the leave-guard is honored, not
  // bypassed) then scroll the card into view once it has mounted. One-shot —
  // `onEmbeddingFocusConsumed` clears the seed in the parent so a later view
  // toggle doesn't re-scroll.
  const embeddingCardRef = useRef<HTMLDivElement>(null);
  // biome-ignore lint/correctness/useExhaustiveDependencies: `embeddingFocus` is the trigger; requestView is stable enough for a one-shot
  useEffect(() => {
    if (!embeddingFocus) return;
    requestView("advanced");
  }, [embeddingFocus]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: `embeddingFocus` and `view` are the only triggers; consume is stable enough for a one-shot
  useEffect(() => {
    if (!embeddingFocus || view !== "advanced") return;
    // Defer the scroll to the next frame so the advanced view (and the card)
    // has rendered. `scrollIntoView` is a no-op in jsdom but safe to call.
    const id = requestAnimationFrame(() => {
      embeddingCardRef.current?.scrollIntoView({ behavior: "smooth", block: "start" });
      onEmbeddingFocusConsumed?.();
    });
    return () => cancelAnimationFrame(id);
  }, [embeddingFocus, view]);

  // Save-bar counts are split per view and key off the applicable PAYLOAD
  // (an invalid form is not saveable, so it doesn't add to the count). The
  // leave-guards key off EDITED so an invalid-but-edited form still blocks
  // navigation. App-level tab leave-guard uses the combined `anyEdited`.
  const basicDirtyCount = [llmPayload, hfPayload, tzPayload].filter(Boolean).length;
  const advancedDirtyCount = [embPayload, mcpPayload].filter(Boolean).length;
  // The destructive vectordb-reset warning must fire ONLY when the pending
  // embedding change actually changes the model id / vector dimension (what
  // the backend's `needs_vectordb_reset` keys off). A dtype / max-seq /
  // tokenizer / multimodal-only edit produces a saveable payload but leaves
  // the index intact, so warning "the vectordb will be reset" there is a
  // false alarm. Gate on the payload too: a null payload won't be saved.
  const advancedResetsVectordb = embPayload !== null && embResetsVectordb;
  // The save bar / blocker modal always promise a sidecar restart for LLM
  // changes. The backend DOES hot-reload some External-only swaps in place, but
  // the frontend can't prove it: a Local target always restarts now (loading
  // the GGUF in a fresh child — an in-process Release→Load crashed Metal), and
  // an External target only hot-reloads when the child already carries the
  // provider's key env, which the frontend can't see. Showing the restart copy
  // is therefore a harmless over-warning (the user may wait less than implied),
  // never a false "no restart". So no `hotReload` flag is passed below.
  const basicEdited = llmEdited || hfEdited || tzEdited;
  // Advanced view holds Embedding (save bar) AND the self-contained
  // Connection / Data location cards — all three must arm the leave-guard.
  const advancedEdited = embEdited || mcpEdited || connEdited || dataRootEdited;
  const anyEdited = basicEdited || advancedEdited;

  // Mirror the aggregate edited flag up to App (drives the tab leave-guard).
  const setDirty = dirty?.setDirty;
  useEffect(() => {
    setDirty?.(anyEdited);
  }, [anyEdited, setDirty]);
  // Clearing the flag on unmount means a tab switch (which unmounts this
  // page and drops the form state) leaves no stale "dirty" behind.
  useEffect(() => {
    return () => setDirty?.(false);
  }, [setDirty]);

  const discardBasic = () => {
    setLlmPayload(null);
    setHfPayload(null);
    setTzPayload(null);
    setLlmEdited(false);
    setHfEdited(false);
    setTzEdited(false);
    setApplyError(null);
    setResetSignal((n) => n + 1);
  };
  const discardAdvanced = () => {
    setEmbPayload(null);
    setEmbEdited(false);
    setMcpPayload(null);
    setMcpEdited(false);
    setApplyError(null);
    setResetSignal((n) => n + 1);
    // Connection / Data location are not part of this batch and reset
    // themselves via their unmount cleanup when the view switches; nothing
    // to clear here.
  };

  // Apply only the payloads owned by the given view, so each view's save
  // bar restarts the sidecar once for its own scope. `apply_settings`
  // accepts null for the untouched scopes.
  const applyScope = async (scope: "basic" | "advanced") => {
    setApplyBusy(true);
    setApplyError(null);
    setBackupPath(null);
    try {
      const res = await applySettings({
        llm: scope === "basic" ? llmPayload : null,
        embedding: scope === "advanced" ? embPayload : null,
        hf_home: scope === "basic" ? hfPayload : null,
        mcp: scope === "advanced" ? mcpPayload : null,
        timezone: scope === "basic" ? tzPayload : null,
      });
      setBackupPath(res.backup_path);
      // Clear only the saved scope's payloads/edited; the other view's
      // unsaved edits survive (its cards re-report after reseed).
      if (scope === "basic") {
        setLlmPayload(null);
        setHfPayload(null);
        setTzPayload(null);
        setLlmEdited(false);
        setHfEdited(false);
        setTzEdited(false);
      } else {
        setEmbPayload(null);
        setEmbEdited(false);
        setMcpPayload(null);
        setMcpEdited(false);
      }
      setResetSignal((n) => n + 1);
      await Promise.all([
        refetch(),
        queryClient.invalidateQueries({ queryKey: ["model-status"] }),
        queryClient.invalidateQueries({ queryKey: ["llm-settings"] }),
        queryClient.invalidateQueries({ queryKey: ["embedding-settings"] }),
        queryClient.invalidateQueries({ queryKey: ["mcp-settings"] }),
        queryClient.invalidateQueries({ queryKey: ["app-settings"] }),
        queryClient.invalidateQueries({ queryKey: ["settings"] }),
        queryClient.invalidateQueries({ queryKey: ["memory-embedding-stats"] }),
        queryClient.invalidateQueries({ queryKey: ["background-job-queue-status"] }),
        queryClient.invalidateQueries({ queryKey: ["reflection-intent-stats"] }),
      ]);
    } catch (e) {
      setApplyError((e as Error).message);
    } finally {
      setApplyBusy(false);
    }
  };

  // Guarded in-component view switch: if the current view has unsaved
  // changes, park the target behind a confirm dialog; otherwise switch
  // immediately (clearing the last save banner).
  const requestView = (next: "basic" | "advanced") => {
    if (next === view) return;
    // Use EDITED (not the saveable payload) so an invalid-but-edited form
    // still triggers the confirm before its input is dropped.
    const currentEdited = view === "basic" ? basicEdited : advancedEdited;
    if (currentEdited) {
      setPendingView(next);
      return;
    }
    setApplyError(null);
    setBackupPath(null);
    setView(next);
  };

  const handleRetry = async () => {
    setRetrying(true);
    try {
      // Re-runs plugin staging + sidecar start + worker apply on the Rust
      // side, then re-reads status. Sidecar (re)start can take a while, so
      // the button stays disabled until both settle.
      await retryModelSetup();
      await Promise.all([modelStatus.refetch(), refetch()]);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setRetrying(false);
    }
  };

  const handlePurge = async () => {
    setBusy(true);
    setError(null);
    try {
      const report = await purgeAllData();
      setConfirming(false);
      await refetch();
      // Surface partial-cleanup warnings (currently only the Keychain entry,
      // which lives outside the data root and may fail to delete on a locked
      // login keychain). Use alert so the user can't dismiss it accidentally
      // — leaving the API key behind is a security concern.
      if (report.warnings.length > 0) {
        alert(t("settings.purge.warningsAlert", { warnings: report.warnings.join("\n\n") }));
      }
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <Toolbar
        title={view === "basic" ? t("settings.title") : t("settings.titleAdvanced")}
        actions={
          view === "advanced" ? (
            <button type="button" className="btn" onClick={() => requestView("basic")}>
              {t("settings.backToBasic")}
            </button>
          ) : undefined
        }
      />
      <div className="content" ref={contentRef}>
        {view === "basic" ? (
          <>
            <LlmProviderCard
              retrying={retrying}
              status={modelStatus.data?.llm}
              onRetry={handleRetry}
              onDirtyChange={reportLlm}
              pendingEmbeddingSettings={embPayload}
              resetSignal={resetSignal}
            />

            <MemoryEmbeddingCard />

            <BackgroundJobQueueCard />

            <HfHomeCard onDirtyChange={reportHf} resetSignal={resetSignal} />

            <TimezoneCard
              onDirtyChange={reportTz}
              resetSignal={resetSignal}
              disabled={connectionConfig.data?.mode === "remote"}
            />

            <LogsCard />

            {applyError && (
              <div className="settings-card" style={{ borderColor: "var(--danger)" }}>
                <div style={{ color: "var(--danger)", fontSize: 12 }}>{applyError}</div>
              </div>
            )}
            {backupPath && (
              <div className="settings-card">
                <div style={{ color: "var(--label-secondary)", fontSize: 11 }}>
                  {t("settings.vectordbBackup.evacuated")} <code>{backupPath}</code>
                  <br />
                  {t("settings.vectordbBackup.manualDelete")}
                </div>
              </div>
            )}

            {/* Entry point into the advanced view. */}
            <div className="settings-card">
              <div className="settings-card-title">{t("settings.advanced.title")}</div>
              <div className="settings-card-desc">{t("settings.advanced.desc")}</div>
              <div className="settings-row">
                <div className="settings-row-label" />
                <button type="button" className="btn" onClick={() => requestView("advanced")}>
                  {t("settings.advanced.open")}
                </button>
              </div>
            </div>
          </>
        ) : (
          <>
            <div className="settings-card" style={{ borderColor: "var(--accent)" }}>
              <div className="settings-card-desc" style={{ marginBottom: 0 }}>
                {t("settings.advanced.notice")}
              </div>
            </div>

            <ConnectionCard
              localJobworkerpUrl={data?.jobworkerp_url ?? null}
              localMemoriesUrl={data?.memories_url ?? null}
              onEditedChange={reportConnEdited}
            />

            <div ref={embeddingCardRef}>
              <EmbeddingProviderCard
                retrying={retrying}
                status={modelStatus.data?.embedding}
                onRetry={handleRetry}
                onDirtyChange={reportEmb}
                onResetsVectordbChange={reportEmbResets}
                resetSignal={resetSignal}
              />
            </div>

            <McpServerCard onDirtyChange={reportMcp} resetSignal={resetSignal} />

            <DataRootCard
              sqlitePath={data?.sqlite_path ?? null}
              lancedbPath={data?.lancedb_path ?? null}
              pluginsPath={data?.plugins_path ?? null}
              logPath={data?.log_path ?? null}
              onEditedChange={reportDataRootEdited}
            />

            <div className="settings-card">
              <div className="settings-card-title">{t("settings.purge.title")}</div>
              <div className="settings-card-desc">{t("settings.purge.desc")}</div>
              <div className="settings-row">
                <div className="settings-row-label">{t("settings.purge.label")}</div>
                <button
                  type="button"
                  className="btn danger"
                  disabled={busy}
                  onClick={() => setConfirming(true)}
                >
                  {t("settings.purge.button")}
                </button>
              </div>
              {error && <div style={{ color: "var(--danger)", fontSize: 11 }}>{error}</div>}
            </div>

            {applyError && (
              <div className="settings-card" style={{ borderColor: "var(--danger)" }}>
                <div style={{ color: "var(--danger)", fontSize: 12 }}>{applyError}</div>
              </div>
            )}
            {backupPath && (
              <div className="settings-card">
                <div style={{ color: "var(--label-secondary)", fontSize: 11 }}>
                  {t("settings.vectordbBackup.evacuated")} <code>{backupPath}</code>
                  <br />
                  {t("settings.vectordbBackup.manualDelete")}
                </div>
              </div>
            )}
          </>
        )}
      </div>

      {/* Save bar lives OUTSIDE the scroll container (`.content`) so it is
          pinned to the foot of the viewport regardless of scroll position —
          editing a card at the top of the page must not hide the save action
          off-screen at the bottom. The bar's scope follows the active view:
          the basic view saves LLM/HF_HOME, the advanced view saves embedding,
          each restarting the sidecar once for its own scope. */}
      {view === "basic" ? (
        <SettingsSaveBar
          dirtyCount={basicDirtyCount}
          resetsVectordb={false}
          saving={applyBusy}
          onDiscard={discardBasic}
          onSave={() => void applyScope("basic")}
        />
      ) : (
        <SettingsSaveBar
          dirtyCount={advancedDirtyCount}
          resetsVectordb={advancedResetsVectordb}
          saving={applyBusy}
          onDiscard={discardAdvanced}
          onSave={() => void applyScope("advanced")}
        />
      )}

      {applyBusy && (
        <SavingBlockerModal
          title={t("settings.savingModal.title")}
          description={
            advancedResetsVectordb
              ? t("settings.savingModal.descWithVectordbReset")
              : t("settings.savingModal.desc")
          }
        />
      )}

      {pendingView && (
        <ConfirmDialog
          title={t("settings.unsavedDialog.title")}
          message={t("settings.unsavedDialog.message")}
          confirmLabel={t("settings.unsavedDialog.confirm")}
          onConfirm={() => {
            if (view === "basic") discardBasic();
            else discardAdvanced();
            setApplyError(null);
            setBackupPath(null);
            setView(pendingView);
            setPendingView(null);
          }}
          onCancel={() => setPendingView(null)}
        />
      )}

      {confirming && (
        <Modal onClose={() => setConfirming(false)} ariaLabel={t("settings.purge.modalTitle")}>
          <div className="modal-head">
            <div className="modal-title">{t("settings.purge.modalTitle")}</div>
          </div>
          <div className="modal-body" style={{ fontSize: 12 }}>
            <p>{t("settings.purge.modalBody")}</p>
            <pre
              style={{
                fontFamily: "var(--font-mono)",
                fontSize: 11,
                marginTop: 8,
                padding: 8,
                background: "var(--secondary-bg)",
                borderRadius: 6,
              }}
            >
              {data?.data_root ?? ""}
            </pre>
            <p style={{ marginTop: 8, color: "var(--danger)" }}>
              {t("settings.purge.modalWarning")}
            </p>
          </div>
          <div className="modal-foot">
            <button
              type="button"
              className="btn"
              onClick={() => setConfirming(false)}
              disabled={busy}
            >
              {t("common.cancel")}
            </button>
            <button type="button" className="btn danger" onClick={handlePurge} disabled={busy}>
              {busy ? t("settings.purge.deleting") : t("settings.purge.confirm")}
            </button>
          </div>
        </Modal>
      )}
    </>
  );
}

function SettingRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="settings-row">
      <div className="settings-row-label">{label}</div>
      <input type="text" value={value} readOnly style={{ flex: 1, opacity: 0.7 }} />
    </div>
  );
}

// "preparing" really means "not cached yet": the model is only fetched once
// the first generation job runs (see the preparing hint in the card), so a
// plain "準備中…" would mislead the user into waiting forever.
const MODEL_STATE_LABEL_KEY: Record<ModelState, string> = {
  preparing: "settings.modelState.preparing",
  ready: "settings.modelState.ready",
  failed: "settings.modelState.failed",
};

const MODEL_STATE_COLOR: Record<ModelState, string> = {
  preparing: "var(--label-secondary)",
  ready: "var(--success, #2ea043)",
  failed: "var(--danger)",
};

function ModelStatusBadge({ state, error }: { state?: ModelState; error: string | null }) {
  const { t } = useTranslation();
  if (!state) {
    return (
      <span style={{ flex: 1, color: "var(--label-tertiary)", fontSize: 12 }}>
        {t("settings.modelState.fetching")}
      </span>
    );
  }
  return (
    <span
      style={{ flex: 1, color: MODEL_STATE_COLOR[state], fontSize: 12, fontWeight: 600 }}
      title={error ?? undefined}
    >
      {t(MODEL_STATE_LABEL_KEY[state])}
    </span>
  );
}

/**
 * A model's identity + preparation state, rendered INSIDE the
 * provider card (LLM / embedding) so "what model" and "is it ready" live in
 * one place instead of a separate read-only card. Both are HF models fetched
 * lazily on first use, so the "preparing = not downloaded yet, run Import to
 * trigger it" guidance is identical; `purpose` names what the model powers so
 * the hint reads naturally for each.
 */
function ModelStatusSection({
  purpose,
  status,
  retrying,
  onRetry,
}: {
  purpose: string;
  status?: ModelStatus;
  retrying: boolean;
  onRetry: () => void;
}) {
  const { t } = useTranslation();
  // Model name/repo come from Rust (resolved from the worker YAML + env), so
  // the display follows a model swap with no frontend edit. Fall back to a
  // dash only if the YAML couldn't be read.
  const name = status?.name ?? "—";
  const repo = status?.repo ?? "—";
  return (
    <>
      <SettingRow label={t("settings.modelStatus.currentModel")} value={name} />
      <SettingRow label={t("settings.modelStatus.provider")} value={repo} />
      <div className="settings-row">
        <div className="settings-row-label">{t("settings.modelStatus.readiness")}</div>
        <ModelStatusBadge state={status?.state} error={status?.error ?? null} />
        {status?.state === "failed" && (
          <button
            type="button"
            className="btn"
            onClick={() => void onRetry()}
            disabled={retrying}
            title={t("settings.modelStatus.retryTitle")}
          >
            {retrying ? t("settings.modelStatus.retrying") : t("settings.modelStatus.retry")}
          </button>
        )}
      </div>
      {status?.state === "preparing" && (
        <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
          {t("settings.modelStatus.preparingHintPre", { purpose })}
          <strong style={{ color: "var(--label-secondary)" }}>
            {" "}
            {t("settings.modelStatus.preparingHintRunImport")}
          </strong>
          {t("settings.modelStatus.preparingHintPost")}
        </div>
      )}
      {status?.state === "ready" && (
        <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
          {t("settings.modelStatus.readyHint", { purpose })}
        </div>
      )}
      {status?.state === "failed" && (
        <div style={{ color: "var(--danger)", fontSize: 11 }}>
          {status.error ?? t("settings.modelStatus.failedDefault")}
          <div style={{ color: "var(--label-tertiary)", marginTop: 2 }}>
            {t("settings.modelStatus.failedHint")}
          </div>
        </div>
      )}
    </>
  );
}

const LLM_MODE_OPTIONS: { value: LlmMode; labelKey: string }[] = [
  { value: "local", labelKey: "settings.llm.modeLocal" },
  { value: "external", labelKey: "settings.llm.modeExternal" },
];

// Mirrors the backend (`llm_presets.rs`): the option list is the `KvCacheType`
// enum order and the default tracks `DEFAULT_KV_CACHE_TYPE` there. The estimate
// math below is duplicated from the Rust `estimate_*_ram_gb` helpers so Settings
// can update the RAM figure live as ctx_size / KV type change without a round-trip.
const KV_CACHE_TYPE_OPTIONS: KvCacheType[] = ["Q4_0", "Q4_1", "IQ4_NL", "Q5_0", "Q5_1", "Q8_0"];
const DEFAULT_KV_CACHE_TYPE: KvCacheType = "Q4_0";

function kvCacheBytesPerElement(type: KvCacheType): number {
  switch (type) {
    // ggml block quantization stores scale/min side data per 32 values.
    case "Q4_0":
    case "IQ4_NL":
      return 18 / 32;
    case "Q4_1":
      return 20 / 32;
    case "Q5_0":
      return 22 / 32;
    case "Q5_1":
      return 24 / 32;
    case "Q8_0":
      return 34 / 32;
  }
}

function estimateKvCacheRamGb(preset: LlmPreset, ctxSize: number, kvType: KvCacheType): number {
  const bytes =
    ctxSize *
    preset.kv_layers *
    (preset.kv_embd_k_gqa + preset.kv_embd_v_gqa) *
    kvCacheBytesPerElement(kvType);
  return bytes / 1024 / 1024 / 1024;
}

function estimateTotalRamGb(preset: LlmPreset, ctxSize: number, kvType: KvCacheType): number {
  return preset.estimated_model_ram_gb + estimateKvCacheRamGb(preset, ctxSize, kvType);
}

function formatRamGb(value: number): string {
  return value.toFixed(1);
}

/**
 * Props shared by the restart-bearing cards (LLM / Embedding / HF_HOME).
 * Each card lifts its dirty state up via `onDirtyChange(payload, edited)`:
 * - `payload`: the applicable save payload, or `null` when the form matches
 *   the persisted value OR the edit is invalid (so it can't be applied).
 * - `edited`: whether the form differs from the persisted value at all,
 *   REGARDLESS of validity. The parent uses this for the leave-guard so an
 *   invalid-but-edited form (e.g. External with an empty model name) still
 *   blocks an unconfirmed navigation away, while the save bar keys off
 *   `payload` (an invalid form is not saveable).
 * `resetSignal` is an incrementing counter that tells the card to re-seed
 * from the server data and clear its dirty state (the "破棄" action).
 */
export interface DirtyReporter<P> {
  onDirtyChange: (payload: P | null, edited: boolean) => void;
  resetSignal: number;
}

/// Parse a number input that may legitimately be `0`. `Number(x) || null`
/// is wrong here — `0 || null` evaluates to `null`, so saving 0 silently
/// reverts to the chat default. NaN (the only result of a non-numeric
/// string) becomes `null` instead.
function parseOptionalNonNegativeNumber(raw: string): number | null {
  if (raw === "") return null;
  const n = Number(raw);
  return Number.isFinite(n) && n >= 0 ? n : null;
}

/** Frontend mirror of `llm_settings::is_valid_hf_repo` — used for early
 *  inline validation so the Save button surfaces a typo before the
 *  sidecar restart. The backend validates again. */
function isValidHfRepoShape(s: string): boolean {
  const parts = s.split("/");
  if (parts.length !== 2) return false;
  const [org, name] = parts;
  if (!org || !name) return false;
  return /^[A-Za-z0-9_.-]+$/.test(org) && /^[A-Za-z0-9_.-]+$/.test(name);
}

export function LlmProviderCard({
  retrying,
  status,
  onRetry,
  onDirtyChange,
  pendingEmbeddingSettings = null,
  resetSignal,
}: {
  retrying: boolean;
  status?: ModelStatus;
  onRetry: () => void;
  pendingEmbeddingSettings?: SetEmbeddingSettingsRequest | null;
} & DirtyReporter<SetLlmSettingsRequest>) {
  const { t } = useTranslation();
  const { data } = useQuery({
    queryKey: ["llm-settings"],
    queryFn: getLlmSettings,
  });
  // Presets are baked into the Rust binary and don't change at runtime;
  // cache forever so a route switch doesn't refetch.
  const { data: presets } = useQuery({
    queryKey: ["llm-presets"],
    queryFn: listLlmPresets,
    staleTime: Number.POSITIVE_INFINITY,
  });
  const { data: embeddingSettings } = useQuery({
    queryKey: ["embedding-settings"],
    queryFn: getEmbeddingSettings,
  });
  const { data: embeddingPresets } = useQuery({
    queryKey: ["embedding-presets"],
    queryFn: listEmbeddingPresets,
    staleTime: Number.POSITIVE_INFINITY,
  });

  const [mode, setMode] = useState<LlmMode>("local");
  const [model, setModel] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [maxTokens, setMaxTokens] = useState("");
  const [temperature, setTemperature] = useState("");
  // Explicit "delete the stored key" intent. Routed through the unified
  // save (api_key = "") rather than an immediate restart so it batches
  // with any other change.
  const [deleteKey, setDeleteKey] = useState(false);
  // Local LLM model selection: empty string until presets load, then the
  // persisted id or the default preset id.
  const [localPresetId, setLocalPresetId] = useState<string>("");
  const [localModelFile, setLocalModelFile] = useState("");
  const [localHfRepo, setLocalHfRepo] = useState("");
  const [localCtxSize, setLocalCtxSize] = useState("");
  const [localKvCacheType, setLocalKvCacheType] = useState<KvCacheType>(DEFAULT_KV_CACHE_TYPE);
  const [showAdvanced, setShowAdvanced] = useState(false);

  const seedFromData = useCallback(() => {
    if (!data) return;
    setMode(data.mode);
    setModel(data.provider_model ?? "");
    setApiKey("");
    setDeleteKey(false);
    setBaseUrl(data.base_url ?? "");
    setMaxTokens(data.max_tokens != null ? String(data.max_tokens) : "");
    setTemperature(data.temperature != null ? String(data.temperature) : "");
    setLocalModelFile(data.local_model_file ?? "");
    setLocalHfRepo(data.local_hf_repo ?? "");
    setLocalCtxSize(data.local_ctx_size != null ? String(data.local_ctx_size) : "");
    setLocalKvCacheType(data.local_kv_cache_type ?? DEFAULT_KV_CACHE_TYPE);
  }, [data]);

  // Re-seed when the server data (re)loads OR the parent fires a discard.
  useEffect(() => {
    seedFromData();
  }, [seedFromData]);
  // biome-ignore lint/correctness/useExhaustiveDependencies: resetSignal is the discard trigger
  useEffect(() => {
    if (resetSignal === 0) return;
    seedFromData();
  }, [resetSignal]);

  // Seed the preset dropdown once presets arrive (data may resolve first).
  // biome-ignore lint/correctness/useExhaustiveDependencies: see seed-once note
  useEffect(() => {
    if (!presets || presets.length === 0) return;
    if (localPresetId !== "") return;
    setLocalPresetId(data?.local_preset_id ?? presets[0]?.id ?? "");
  }, [data, presets]);
  // Discard must reset the preset dropdown too (the effect above only
  // seeds while it is still "").
  // biome-ignore lint/correctness/useExhaustiveDependencies: resetSignal is the discard trigger
  useEffect(() => {
    if (resetSignal === 0) return;
    setLocalPresetId(data?.local_preset_id ?? presets?.[0]?.id ?? "");
  }, [resetSignal]);
  // Re-sync the dropdown when the PERSISTED preset id changes on the
  // server. After a save, the parent fires resetSignal first (which seeds
  // localPresetId from the still-stale `data`), then the refetch resolves
  // with the new value; the seed-once effect above bails (localPresetId !==
  // "") so without this the dropdown would keep the old preset and re-report
  // dirty right after a successful save. A ref tracks the last persisted id
  // so this only fires on a genuine server change, never clobbering the
  // user's in-progress dropdown choice.
  const lastPersistedPresetId = useRef<string | null | undefined>(undefined);
  useEffect(() => {
    const persisted = data?.local_preset_id ?? null;
    if (lastPersistedPresetId.current === undefined) {
      // First observation: record the baseline without overriding the seed.
      lastPersistedPresetId.current = persisted;
      return;
    }
    if (persisted !== lastPersistedPresetId.current) {
      lastPersistedPresetId.current = persisted;
      setLocalPresetId(persisted ?? presets?.[0]?.id ?? "");
    }
  }, [data, presets]);

  const activePreset: LlmPreset | undefined = presets?.find((p) => p.id === localPresetId);
  const isCustomPreset = localPresetId === CUSTOM_LLM_PRESET_ID;
  const activeCtxSize =
    parseOptionalNonNegativeNumber(localCtxSize) ?? activePreset?.recommended_ctx_size ?? 262_144;
  // RAM figures only render for a known preset; custom presets lack the
  // structural fields needed to estimate. Reuse the already-computed KV figure
  // for the total instead of recomputing it.
  const ramPreset = activePreset && !isCustomPreset ? activePreset : null;
  const activeKvRamGb = ramPreset
    ? estimateKvCacheRamGb(ramPreset, activeCtxSize, localKvCacheType)
    : null;
  const activeTotalRamGb =
    ramPreset && activeKvRamGb != null ? ramPreset.estimated_model_ram_gb + activeKvRamGb : null;
  // The pending (unsaved, wizard) selection wins over the persisted one; both
  // fall back to the default preset (first row), mirroring the backend.
  const activeEmbeddingSource = pendingEmbeddingSettings ?? embeddingSettings;
  const activeEmbeddingPresetId = activeEmbeddingSource
    ? (activeEmbeddingSource.preset_id ?? embeddingPresets?.[0]?.id ?? null)
    : null;
  const activeEmbeddingPreset = embeddingPresets?.find((p) => p.id === activeEmbeddingPresetId);
  const activeEmbeddingRamGb =
    activeEmbeddingPresetId && activeEmbeddingPresetId !== CUSTOM_EMBEDDING_PRESET_ID
      ? (activeEmbeddingPreset?.estimated_ram_gb ?? null)
      : null;
  const activeCombinedRamGb =
    activeTotalRamGb != null && activeEmbeddingRamGb != null
      ? activeTotalRamGb + activeEmbeddingRamGb
      : null;

  // Build the save payload mirroring the backend contract. Returns null
  // when the form matches the persisted value (so the card is "clean").
  const buildPayload = useCallback((): SetLlmSettingsRequest | null => {
    if (!data) return null;
    const isLocal = mode === "local";
    // api_key contract: null = no change, "" = delete, "x" = set.
    const apiKeyPayload: string | null = deleteKey
      ? ""
      : mode === "external" && apiKey
        ? apiKey
        : null;
    // Preserve the inactive mode's persisted fields verbatim so a save in
    // one mode doesn't blank the other mode's state.
    const payload: SetLlmSettingsRequest = {
      mode,
      provider_model: isLocal ? (data.provider_model ?? null) : model.trim() || null,
      api_key: apiKeyPayload,
      base_url: isLocal ? (data.base_url ?? null) : baseUrl.trim() || null,
      max_tokens: parseOptionalNonNegativeNumber(maxTokens),
      temperature: parseOptionalNonNegativeNumber(temperature),
      local_preset_id: isLocal ? localPresetId || null : (data.local_preset_id ?? null),
      local_model_file: isLocal
        ? isCustomPreset && localModelFile.trim()
          ? localModelFile.trim()
          : null
        : (data.local_model_file ?? null),
      local_hf_repo: isLocal
        ? isCustomPreset && localHfRepo.trim()
          ? localHfRepo.trim()
          : null
        : (data.local_hf_repo ?? null),
      local_ctx_size: isLocal
        ? parseOptionalNonNegativeNumber(localCtxSize)
        : (data.local_ctx_size ?? null),
      local_kv_cache_type: isLocal ? localKvCacheType : (data.local_kv_cache_type ?? null),
    };
    // Dirty check: compare each field against the persisted snapshot. The
    // api_key is dirty only when a delete is requested or a new key typed
    // (a Keychain-stored key is opaque here, so we can't diff its value).
    // The local_* expected-clean values mirror the payload's own mode-gated
    // derivation: in Local mode the preset seeds to the default when the
    // persisted id is null, whereas in External mode the payload passes the
    // persisted local_* through verbatim (no default fallback).
    const cleanPresetId = isLocal
      ? (data.local_preset_id ?? presets?.[0]?.id ?? null)
      : (data.local_preset_id ?? null);
    const cleanKvCacheType = isLocal
      ? (data.local_kv_cache_type ?? DEFAULT_KV_CACHE_TYPE)
      : (data.local_kv_cache_type ?? null);
    const clean =
      payload.mode === data.mode &&
      payload.provider_model === (data.provider_model ?? null) &&
      payload.api_key === null &&
      payload.base_url === (data.base_url ?? null) &&
      payload.max_tokens === (data.max_tokens ?? null) &&
      payload.temperature === (data.temperature ?? null) &&
      payload.local_preset_id === cleanPresetId &&
      payload.local_model_file === (data.local_model_file ?? null) &&
      payload.local_hf_repo === (data.local_hf_repo ?? null) &&
      payload.local_ctx_size === (data.local_ctx_size ?? null) &&
      payload.local_kv_cache_type === cleanKvCacheType;
    return clean ? null : payload;
  }, [
    data,
    mode,
    apiKey,
    deleteKey,
    model,
    baseUrl,
    maxTokens,
    temperature,
    localPresetId,
    localModelFile,
    localHfRepo,
    localCtxSize,
    localKvCacheType,
    isCustomPreset,
    presets,
  ]);

  const externalIncomplete = mode === "external" && model.trim() === "";
  // Inline custom-mode validation: backend rejects the same shapes; gating
  // the dirty report prevents a needless sidecar restart.
  const customIncomplete =
    mode === "local" &&
    isCustomPreset &&
    (localHfRepo.trim() === "" ||
      !isValidHfRepoShape(localHfRepo.trim()) ||
      localModelFile.trim() === "" ||
      !localModelFile.trim().toLowerCase().endsWith(".gguf"));

  // Report (payload, edited) upward whenever the form changes.
  // - `built !== null` means the form differs from the persisted value
  //   (buildPayload returns null only when clean) → that IS "edited",
  //   regardless of validity, so an invalid edit still guards navigation.
  // - The saveable payload is gated on validity so the save bar can't
  //   include a broken LLM change in the batch.
  useEffect(() => {
    const built = buildPayload();
    const edited = built !== null;
    const payload = externalIncomplete || customIncomplete ? null : built;
    onDirtyChange(payload, edited);
  }, [buildPayload, externalIncomplete, customIncomplete, onDirtyChange]);

  return (
    <div className="settings-card">
      <div className="settings-card-title">{t("settings.llm.title")}</div>
      <div className="settings-card-desc">{t("settings.llm.desc")}</div>
      <ModelStatusSection
        purpose={t("settings.llm.modelPurpose")}
        status={status}
        retrying={retrying}
        onRetry={onRetry}
      />
      <div className="settings-row">
        <div className="settings-row-label">{t("settings.llm.backend")}</div>
        <div className="segment">
          {LLM_MODE_OPTIONS.map((opt) => (
            <button
              key={opt.value}
              type="button"
              className={`segment-btn${mode === opt.value ? " active" : ""}`}
              onClick={() => setMode(opt.value)}
            >
              {t(opt.labelKey)}
            </button>
          ))}
        </div>
      </div>

      {mode === "external" && (
        <>
          <div className="settings-row">
            <div className="settings-row-label">{t("settings.llm.modelName")}</div>
            <input
              type="text"
              value={model}
              placeholder="gpt-4o / claude-sonnet-4-20250514 / gemini-2.5-flash"
              onChange={(e) => setModel(e.target.value)}
              style={{ flex: 1 }}
            />
          </div>
          <div className="settings-row">
            <div className="settings-row-label">{t("settings.llm.apiKey")}</div>
            <input
              type="password"
              value={apiKey}
              placeholder={
                deleteKey
                  ? t("settings.llm.apiKeyPlaceholderDelete")
                  : data?.api_key_set
                    ? t("settings.llm.apiKeyPlaceholderStored")
                    : "sk-..."
              }
              onChange={(e) => {
                setApiKey(e.target.value);
                // Typing a key cancels a pending delete.
                if (e.target.value) setDeleteKey(false);
              }}
              disabled={deleteKey}
              style={{ flex: 1 }}
            />
            {data?.api_key_set &&
              (deleteKey ? (
                <button
                  type="button"
                  className="btn"
                  onClick={() => setDeleteKey(false)}
                  disabled={retrying}
                  style={{ marginLeft: 8 }}
                  title={t("settings.llm.cancelDeleteKeyTitle")}
                >
                  {t("settings.llm.cancelDeleteKey")}
                </button>
              ) : (
                <button
                  type="button"
                  className="btn"
                  onClick={() => setDeleteKey(true)}
                  disabled={retrying}
                  style={{ marginLeft: 8 }}
                  title={t("settings.llm.deleteKeyTitle")}
                >
                  {t("settings.llm.deleteKey")}
                </button>
              ))}
          </div>
          {deleteKey && (
            <div style={{ color: "var(--danger)", fontSize: 11, marginTop: 2 }}>
              {t("settings.llm.deleteKeyWarning")}
            </div>
          )}
          <div className="settings-row">
            <div className="settings-row-label">Base URL</div>
            <input
              type="text"
              value={baseUrl}
              placeholder={t("settings.llm.baseUrlPlaceholder")}
              onChange={(e) => setBaseUrl(e.target.value)}
              style={{ flex: 1 }}
            />
          </div>
          <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
            {t("settings.llm.apiKeyHint")}
          </div>
          <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 4 }}>
            <strong style={{ color: "var(--label-secondary)" }}>
              {t("settings.llm.proxyNoteLabel")}
            </strong>{" "}
            {t("settings.llm.proxyNotePre")} <code style={{ fontSize: 10 }}>openai::</code>{" "}
            {t("settings.llm.proxyNoteMid")} ({t("settings.llm.proxyNoteExample")}{" "}
            <code style={{ fontSize: 10 }}>openai::claude-sonnet-4-20250514</code>)
            {t("settings.llm.proxyNotePost")}
          </div>
        </>
      )}

      {/* Chat-only generation overrides — applied by `chat.rs::build_chat_args`
          regardless of mode (Local llama-cpp / External genai both honour
          them). Kept OUTSIDE the External-only block so a user who switches
          back to Local can still inspect / clear values they previously set:
          hiding the inputs while still persisting their state was the bug
          reported in the latest review. */}
      <div className="settings-row">
        <div className="settings-row-label">{t("settings.llm.maxTokensChat")}</div>
        <input
          type="number"
          value={maxTokens}
          placeholder="4000"
          onChange={(e) => setMaxTokens(e.target.value)}
          style={{ width: 100 }}
        />
        <div className="settings-row-label" style={{ marginLeft: 16 }}>
          {t("settings.llm.temperatureChat")}
        </div>
        <input
          type="number"
          value={temperature}
          placeholder="0.3"
          step="0.1"
          min="0"
          max="2"
          onChange={(e) => setTemperature(e.target.value)}
          style={{ width: 80 }}
        />
      </div>
      <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
        {t("settings.llm.genParamsHintPre")} <strong>{t("settings.llm.genParamsHintChat")}</strong>{" "}
        {t("settings.llm.genParamsHintPost")}
      </div>

      {mode === "local" && (
        <>
          <div className="settings-row">
            <div className="settings-row-label">{t("settings.llm.preset")}</div>
            <select
              value={localPresetId}
              onChange={(e) => {
                setLocalPresetId(e.target.value);
                // Switching presets clears the user's ctx_size override
                // so the new preset's `recommended_ctx_size` placeholder
                // is what the model actually sees (else they could ship
                // a 262k override down to a 32k preset by accident).
                setLocalCtxSize("");
              }}
              style={{ flex: 1 }}
              disabled={!presets || presets.length === 0}
            >
              {presets?.map((p) => (
                <option key={p.id} value={p.id}>
                  {t("settings.llm.presetOption", {
                    name: p.display_name,
                    ram: formatRamGb(
                      estimateTotalRamGb(
                        p,
                        parseOptionalNonNegativeNumber(localCtxSize) ?? p.recommended_ctx_size,
                        localKvCacheType,
                      ),
                    ),
                  })}
                </option>
              ))}
              <option value={CUSTOM_LLM_PRESET_ID}>{t("settings.llm.customOption")}</option>
            </select>
          </div>
          {!isCustomPreset && activePreset && (
            <>
              <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
                {t(activePreset.description)}
              </div>
              {activeKvRamGb != null && activeTotalRamGb != null && (
                <div className="settings-ram-estimate">
                  <div>
                    {t("settings.llm.ramEstimate", {
                      model: formatRamGb(activePreset.estimated_model_ram_gb),
                      kv: formatRamGb(activeKvRamGb),
                      total: formatRamGb(activeTotalRamGb),
                      ctx: activeCtxSize,
                      kvType: localKvCacheType,
                    })}
                  </div>
                  <div className="settings-ram-estimate-total">
                    {activeCombinedRamGb != null && activeEmbeddingRamGb != null
                      ? t("settings.llm.ramEstimateWithEmbedding", {
                          embedding: formatRamGb(activeEmbeddingRamGb),
                          total: formatRamGb(activeCombinedRamGb),
                        })
                      : t("settings.llm.embeddingRamUnavailable")}
                  </div>
                </div>
              )}
            </>
          )}
          {isCustomPreset && (
            <>
              <div className="settings-row">
                <div className="settings-row-label">HF repo</div>
                <input
                  type="text"
                  value={localHfRepo}
                  placeholder="unsloth/Qwen3.5-9B-GGUF"
                  onChange={(e) => setLocalHfRepo(e.target.value)}
                  style={{ flex: 1 }}
                />
              </div>
              <div className="settings-row">
                <div className="settings-row-label">{t("settings.llm.ggufFile")}</div>
                <input
                  type="text"
                  value={localModelFile}
                  placeholder="Qwen3.5-9B-UD-Q4_K_XL.gguf"
                  onChange={(e) => setLocalModelFile(e.target.value)}
                  style={{ flex: 1 }}
                />
              </div>
              <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
                {t("settings.llm.customHintPre")} (<code style={{ fontSize: 10 }}>org/name</code>)
                {t("settings.llm.customHintMid")} <code style={{ fontSize: 10 }}>.gguf</code>{" "}
                {t("settings.llm.customHintPost")}
              </div>
              <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
                {t("settings.llm.customRamUnavailable")}
              </div>
            </>
          )}
          <div className="settings-row">
            <div className="settings-row-label" />
            <button
              type="button"
              className="btn"
              onClick={() => setShowAdvanced((v) => !v)}
              style={{ fontSize: 11 }}
            >
              {showAdvanced
                ? t("settings.llm.advancedToggleOpen")
                : t("settings.llm.advancedToggleClosed")}
            </button>
          </div>
          {showAdvanced && (
            <>
              <div className="settings-row">
                <div className="settings-row-label">ctx_size</div>
                <input
                  type="number"
                  value={localCtxSize}
                  placeholder={activePreset ? String(activePreset.recommended_ctx_size) : "262144"}
                  min={activePreset?.min_ctx_size ?? 512}
                  onChange={(e) => setLocalCtxSize(e.target.value)}
                  style={{ width: 140 }}
                />
                <span style={{ marginLeft: 8, color: "var(--label-tertiary)", fontSize: 11 }}>
                  {t("settings.llm.ctxSizeHint", {
                    value: activePreset ? activePreset.recommended_ctx_size : 262144,
                  })}
                </span>
              </div>
              <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
                {t("settings.llm.ctxSizeNote")}
              </div>
              <div className="settings-row">
                <div className="settings-row-label">{t("settings.llm.kvCacheType")}</div>
                <select
                  value={localKvCacheType}
                  onChange={(e) => setLocalKvCacheType(e.target.value as KvCacheType)}
                  style={{ width: 140 }}
                >
                  {KV_CACHE_TYPE_OPTIONS.map((type) => (
                    <option key={type} value={type}>
                      {type}
                    </option>
                  ))}
                </select>
                <span style={{ marginLeft: 8, color: "var(--label-tertiary)", fontSize: 11 }}>
                  {t("settings.llm.kvCacheTypeHint")}
                </span>
              </div>
            </>
          )}
          <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
            {t("settings.llm.localHint")}
          </div>
        </>
      )}

      {(externalIncomplete || customIncomplete) && (
        <div style={{ color: "var(--danger)", fontSize: 11 }}>
          {externalIncomplete
            ? t("settings.llm.externalIncomplete")
            : t("settings.llm.customIncomplete")}
        </div>
      )}
    </div>
  );
}

const CONNECTION_MODES: { value: ConnectionMode; labelKey: string }[] = [
  { value: "local", labelKey: "settings.connection.modeLocal" },
  { value: "remote", labelKey: "settings.connection.modeRemote" },
];

/**
 * Connection-target override. Local mode shows the live sidecar
 * URLs read-only (the dynamic ports are never persisted); remote mode
 * exposes editable URL fields persisted via set_connection_config.
 */
export function ConnectionCard({
  localJobworkerpUrl,
  localMemoriesUrl,
  onEditedChange,
}: {
  localJobworkerpUrl: string | null;
  localMemoriesUrl: string | null;
  /** Reports whether the form differs from the persisted config, so the
   *  parent can arm its leave-guards. This card saves itself (not via the
   *  unified save bar), so only the edited flag is lifted — no payload. */
  onEditedChange?: (edited: boolean) => void;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { data } = useQuery({ queryKey: ["connection-config"], queryFn: getConnectionConfig });

  const [mode, setMode] = useState<ConnectionMode>("local");
  const [jobworkerpUrl, setJobworkerpUrl] = useState("");
  const [memoriesUrl, setMemoriesUrl] = useState("");
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  // Seed the form from the persisted config once it loads.
  useEffect(() => {
    if (!data) return;
    setMode(data.mode);
    setJobworkerpUrl(data.remote_jobworkerp_url ?? "");
    setMemoriesUrl(data.remote_memories_url ?? "");
  }, [data]);

  const remoteIncomplete =
    mode === "remote" && (jobworkerpUrl.trim() === "" || memoriesUrl.trim() === "");

  // Dirty when the form differs from the persisted config. Compared against
  // the same normalisation `handleSave` sends (trim; local mode treats blank
  // URLs as null) so a no-op edit doesn't arm the guard. Until `data` loads
  // there is nothing to diff against, so it's clean.
  const edited =
    data != null &&
    (mode !== data.mode ||
      jobworkerpUrl.trim() !== (data.remote_jobworkerp_url ?? "") ||
      memoriesUrl.trim() !== (data.remote_memories_url ?? ""));
  useEffect(() => {
    onEditedChange?.(edited);
  }, [edited, onEditedChange]);
  // Clear the flag on unmount (view switch / tab change) so a stale "edited"
  // doesn't keep the guard armed after the card is gone.
  useEffect(() => {
    return () => onEditedChange?.(false);
  }, [onEditedChange]);

  const currentConnectionPayload = () => ({
    mode,
    remote_jobworkerp_url: mode === "remote" ? jobworkerpUrl.trim() : jobworkerpUrl.trim() || null,
    remote_memories_url: mode === "remote" ? memoriesUrl.trim() : memoriesUrl.trim() || null,
  });

  const handleSave = async () => {
    setSaving(true);
    setError(null);
    setTestResult(null);
    setSaved(false);
    try {
      await setConnectionConfig(currentConnectionPayload());
      setSaved(true);
      // The connection target changed, so every server-derived cache (threads,
      // memories, summaries, reflections, personality, search) now points at
      // the wrong instance. Invalidate everything so the next render refetches
      // from the new target — listing one instance's threads while the
      // detail/link queries hit another is exactly the half-broken state this
      // avoids. The connection-config / settings snapshots are refetched too,
      // which is harmless (they return the correct current values).
      await queryClient.invalidateQueries();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setSaving(false);
    }
  };

  const handleTest = async () => {
    setTesting(true);
    setError(null);
    setTestResult(null);
    try {
      await testConnectionConfig(currentConnectionPayload());
      setTestResult(t("settings.connection.testOk"));
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setTesting(false);
    }
  };

  return (
    <div className="settings-card">
      <div className="settings-card-title">{t("settings.connection.title")}</div>
      <div className="settings-card-desc">{t("settings.connection.desc")}</div>
      <div className="settings-row">
        <div className="settings-row-label">{t("settings.connection.target")}</div>
        <div className="segment">
          {CONNECTION_MODES.map((opt) => (
            <button
              key={opt.value}
              type="button"
              className={`segment-btn${mode === opt.value ? " active" : ""}`}
              onClick={() => setMode(opt.value)}
            >
              {t(opt.labelKey)}
            </button>
          ))}
        </div>
      </div>

      {mode === "local" ? (
        <>
          <SettingRow
            label="jobworkerp"
            value={localJobworkerpUrl ?? t("settings.connection.starting")}
          />
          <SettingRow
            label="memories"
            value={localMemoriesUrl ?? t("settings.connection.starting")}
          />
        </>
      ) : (
        <>
          <div className="settings-row">
            <div className="settings-row-label">jobworkerp</div>
            <input
              type="text"
              value={jobworkerpUrl}
              placeholder="http://host:9000"
              onChange={(e) => setJobworkerpUrl(e.target.value)}
              style={{ flex: 1 }}
            />
          </div>
          <div className="settings-row">
            <div className="settings-row-label">memories</div>
            <input
              type="text"
              value={memoriesUrl}
              placeholder="http://host:9010"
              onChange={(e) => setMemoriesUrl(e.target.value)}
              style={{ flex: 1 }}
            />
          </div>
        </>
      )}

      <div className="settings-row">
        <div className="settings-row-label" />
        <button
          type="button"
          className="btn primary"
          onClick={() => void handleSave()}
          disabled={saving || remoteIncomplete}
        >
          {saving ? t("settings.connection.saving") : t("settings.connection.save")}
        </button>
        {saved && !error && (
          <span style={{ color: "var(--success, #2ea043)", fontSize: 11, marginLeft: 8 }}>
            {t("settings.connection.saved")}
          </span>
        )}
      </div>
      {mode === "remote" && (
        <div className="settings-row">
          <div className="settings-row-label" />
          <button
            type="button"
            className="btn"
            onClick={() => void handleTest()}
            disabled={testing || remoteIncomplete}
          >
            {testing ? t("settings.connection.testing") : t("settings.connection.test")}
          </button>
          {testResult && !error && (
            <span style={{ color: "var(--success, #2ea043)", fontSize: 11, marginLeft: 8 }}>
              {testResult}
            </span>
          )}
        </div>
      )}
      {error && <div style={{ color: "var(--danger)", fontSize: 11 }}>{error}</div>}
    </div>
  );
}

const LOG_SOURCES: { value: LogSource; label: string }[] = [
  // `app` first / default: it carries the memories-import child output, so it's
  // where an import failure (e.g. a remote TLS error) shows up.
  { value: "app", label: "app" },
  { value: "jobworkerp", label: "jobworkerp" },
  { value: "memories", label: "memories" },
];

const LOG_STREAMS: { value: LogStream; label: string }[] = [
  { value: "stdout", label: "stdout" },
  { value: "stderr", label: "stderr" },
];

/**
 * View logs in-app for troubleshooting. Reads only the tail (the Rust
 * command caps at 1 MiB) so a multi-MB log can't freeze the UI; a manual
 * refresh button is enough for the MVP (no live streaming). `app` is Lookback's
 * own combined log (single file — the stream selector is hidden for it).
 */
function LogsCard() {
  const { t } = useTranslation();
  const [source, setSource] = useState<LogSource>("app");
  const [stream, setStream] = useState<LogStream>("stdout");
  // The app log is a single file; the stream selector is meaningless there.
  const hasStreams = source !== "app";

  const { data, refetch, isFetching, error } = useQuery({
    queryKey: ["sidecar-log", source, stream],
    queryFn: () => readSidecarLog(source, stream),
  });

  return (
    <div className="settings-card">
      <div className="settings-card-title">{t("settings.logs.title")}</div>
      <div className="settings-card-desc">{t("settings.logs.desc")}</div>
      <div className="settings-row">
        <div className="settings-row-label">{t("settings.logs.process")}</div>
        <div className="segment">
          {LOG_SOURCES.map((opt) => (
            <button
              key={opt.value}
              type="button"
              className={`segment-btn${source === opt.value ? " active" : ""}`}
              onClick={() => setSource(opt.value)}
            >
              {opt.label}
            </button>
          ))}
        </div>
      </div>
      <div className="settings-row">
        <div className="settings-row-label">{t("settings.logs.stream")}</div>
        {hasStreams ? (
          <div className="segment">
            {LOG_STREAMS.map((opt) => (
              <button
                key={opt.value}
                type="button"
                className={`segment-btn${stream === opt.value ? " active" : ""}`}
                onClick={() => setStream(opt.value)}
              >
                {opt.label}
              </button>
            ))}
          </div>
        ) : (
          <span style={{ fontSize: 11, color: "var(--label-tertiary)" }}>
            {t("settings.logs.singleFile")}
          </span>
        )}
        <button
          type="button"
          className="btn"
          onClick={() => void refetch()}
          disabled={isFetching}
          style={{ marginLeft: 8 }}
        >
          {isFetching ? t("settings.logs.loading") : t("common.reload")}
        </button>
      </div>
      {error && (
        <div style={{ color: "var(--danger)", fontSize: 11 }}>{(error as Error).message}</div>
      )}
      {data?.truncated && (
        <div style={{ color: "var(--label-tertiary)", fontSize: 11 }}>
          {t("settings.logs.tailOnly")}
        </div>
      )}
      <pre
        style={{
          fontFamily: "var(--font-mono)",
          fontSize: 11,
          marginTop: 8,
          padding: 8,
          maxHeight: 320,
          overflow: "auto",
          whiteSpace: "pre-wrap",
          wordBreak: "break-all",
          background: "var(--secondary-bg)",
          borderRadius: 6,
        }}
      >
        {data?.content ? data.content : t("settings.logs.empty")}
      </pre>
    </div>
  );
}

/**
 * Memory (summary / thread body) embedding coverage + manual re-dispatch.
 * Sits next to the Embedding model card because the two answer the same
 * question — "is search working?" — from different angles (model readiness vs.
 * index coverage). The backend `MemoryVectorService.RedispatchEmbeddings` scans
 * the RDB and re-issues the embedding job idempotently, so a single button
 * covers both "fill the gaps after import" and "rebuild after switching
 * embedding model"; a confirm() guards against accidental long-running runs.
 *
 * Deliberately no manual "更新" button: TanStack Query's default
 * `refetchOnWindowFocus: false` is global, but invalidating after the
 * dispatch action is enough — same pattern the Connection/LLM Provider
 * cards already use.
 */
/**
 * Embedding model selection card. Mirrors `LlmProviderCard` (preset
 * dropdown + Custom row + save → sidecar restart) but for the embedding
 * runner. Changing the vector dimension renames the existing lancedb
 * into a timestamped backup or deletes it; the choice is exposed as a
 * checkbox so a user comfortable with the previous behaviour can opt
 * out.
 *
 * Still editable when `connection.mode === "remote"`: local article embeddings
 * are not regenerated then, but semantic query embedding must match the remote
 * memories vector space.
 */
export function EmbeddingProviderCard({
  retrying,
  status,
  onRetry,
  onDirtyChange,
  onResetsVectordbChange,
  suppressResetWarning = false,
  preferredDefaultLanguage,
  resetSignal,
}: {
  retrying: boolean;
  status?: ModelStatus;
  onRetry: () => void;
  /** Reports whether the pending change will actually reset the vectordb
   *  (model id / vector dimension changes vs the persisted effective
   *  runtime). Mirrors the backend's `needs_vectordb_reset` so the parent's
   *  destructive warning only fires for a real index reset. */
  onResetsVectordbChange?: (resets: boolean) => void;
  /** First-run setup starts with an empty vectordb, so reset warnings would be noise. */
  suppressResetWarning?: boolean;
  /** First-run only: choose a language-recommended preset when no selection
   *  has been persisted. Normal Settings deliberately leaves this unset. */
  preferredDefaultLanguage?: string;
} & DirtyReporter<SetEmbeddingSettingsRequest>) {
  const { t } = useTranslation();
  const { data, isLoading } = useQuery({
    queryKey: ["embedding-settings"],
    queryFn: getEmbeddingSettings,
  });
  const presets = useQuery({
    queryKey: ["embedding-presets"],
    queryFn: listEmbeddingPresets,
    staleTime: Number.POSITIVE_INFINITY,
  });

  const remote = data?.connection_remote ?? false;
  const initialPresetId = data?.preset_id ?? null;
  const [presetId, setPresetId] = useState<string>(initialPresetId ?? presets.data?.[0]?.id ?? "");
  const [customModel, setCustomModel] = useState(data?.custom_model_id ?? "");
  const [customTokenizer, setCustomTokenizer] = useState(data?.custom_tokenizer_id ?? "");
  const [customVectorSize, setCustomVectorSize] = useState<string>(
    data?.custom_vector_size != null ? String(data.custom_vector_size) : "",
  );
  const [customDtype, setCustomDtype] = useState<string>(data?.custom_dtype ?? "F16");
  const [customMaxSeq, setCustomMaxSeq] = useState<string>(
    data?.custom_max_sequence_length != null ? String(data.custom_max_sequence_length) : "",
  );
  const [customMultimodal, setCustomMultimodal] = useState<boolean>(
    data?.custom_is_multimodal ?? false,
  );
  const [evacuate, setEvacuate] = useState<boolean>(true);

  // Stable reference so a render churn during loading doesn't force the
  // reseed effect to fire repeatedly with the same data.
  const presetList: EmbeddingPreset[] = useMemo(() => presets.data ?? [], [presets.data]);
  const defaultPresetId = useMemo(
    () =>
      presetList.find((preset) =>
        preset.recommended_languages?.some(
          (language) => language.toLowerCase() === preferredDefaultLanguage?.toLowerCase(),
        ),
      )?.id ??
      presetList[0]?.id ??
      "",
    [presetList, preferredDefaultLanguage],
  );

  const seedFromData = useCallback(() => {
    if (!data) return;
    setPresetId(data.preset_id ?? defaultPresetId);
    setCustomModel(data.custom_model_id ?? "");
    setCustomTokenizer(data.custom_tokenizer_id ?? "");
    setCustomVectorSize(data.custom_vector_size != null ? String(data.custom_vector_size) : "");
    setCustomDtype(data.custom_dtype ?? "F16");
    setCustomMaxSeq(
      data.custom_max_sequence_length != null ? String(data.custom_max_sequence_length) : "",
    );
    setCustomMultimodal(data.custom_is_multimodal ?? false);
    setEvacuate(true);
  }, [data, defaultPresetId]);

  // Re-seed when the server data (re)loads OR the parent fires a discard.
  useEffect(() => {
    seedFromData();
  }, [seedFromData]);
  // biome-ignore lint/correctness/useExhaustiveDependencies: resetSignal is the discard trigger
  useEffect(() => {
    if (resetSignal === 0) return;
    seedFromData();
  }, [resetSignal]);

  // Late-arriving presets: if `data` resolved first (preset_id: null) the
  // initial seed above ran against an empty presetList and left presetId
  // as "". When the preset list then arrives, fill in the default preset
  // — but only when the user hasn't already touched the dropdown.
  // biome-ignore lint/correctness/useExhaustiveDependencies: see above
  useEffect(() => {
    if (presetList.length === 0) return;
    if (presetId !== "") return;
    if (data?.preset_id != null) return;
    setPresetId(defaultPresetId);
  }, [presetList, data, defaultPresetId]);

  const selectedPreset = useMemo(
    () => presetList.find((p) => p.id === presetId),
    [presetList, presetId],
  );
  const isCustom = presetId === CUSTOM_EMBEDDING_PRESET_ID;
  const effective = data?.effective;

  const disabled = retrying || isLoading;

  // Custom requires BOTH model id and vector size. Vector size has no
  // safe default because the active preset dim can mismatch the model's
  // actual output dim, making every embedding upsert fail. `parseInt("")`
  // and `parseInt("  ")` both return NaN, so the trim check is redundant.
  const customRequiredOk =
    !isCustom || (customModel.trim() !== "" && Number.parseInt(customVectorSize, 10) > 0);

  // Build the save payload, or null when the card is clean / cannot be
  // applied (still loading or an incomplete custom row).
  const buildPayload = useCallback((): SetEmbeddingSettingsRequest | null => {
    if (!data) return null;
    const customOk =
      !isCustom || (customModel.trim() !== "" && Number.parseInt(customVectorSize, 10) > 0);
    if (!customOk) return null;
    const payload: SetEmbeddingSettingsRequest = {
      preset_id: presetId === "" ? null : presetId,
      custom_model_id: isCustom ? customModel.trim() || null : null,
      custom_tokenizer_id: isCustom ? customTokenizer.trim() || null : null,
      custom_vector_size:
        isCustom && customVectorSize ? Number.parseInt(customVectorSize, 10) || null : null,
      custom_dtype: isCustom ? customDtype : null,
      custom_max_sequence_length:
        isCustom && customMaxSeq ? Number.parseInt(customMaxSeq, 10) || null : null,
      custom_is_multimodal: isCustom ? customMultimodal : null,
      evacuate_vectordb: evacuate,
    };
    // Dirty check against the persisted snapshot. `evacuate_vectordb` is a
    // save-time policy, not persisted state, so it is excluded from the
    // diff (toggling it alone is not "dirty").
    const clean =
      payload.preset_id === (data.preset_id ?? (defaultPresetId || null)) &&
      payload.custom_model_id === (data.custom_model_id ?? null) &&
      payload.custom_tokenizer_id === (data.custom_tokenizer_id ?? null) &&
      payload.custom_vector_size === (data.custom_vector_size ?? null) &&
      payload.custom_dtype === (isCustom ? (data.custom_dtype ?? "F16") : null) &&
      payload.custom_max_sequence_length === (data.custom_max_sequence_length ?? null) &&
      payload.custom_is_multimodal === (isCustom ? (data.custom_is_multimodal ?? false) : null);
    return clean ? null : payload;
  }, [
    data,
    isCustom,
    presetId,
    customModel,
    customTokenizer,
    customVectorSize,
    customDtype,
    customMaxSeq,
    customMultimodal,
    evacuate,
    defaultPresetId,
  ]);

  // Whether the form differs from the persisted value, IGNORING the
  // custom-incomplete gate (so a half-typed custom row still guards
  // navigation). Mirrors buildPayload's clean comparison.
  const edited = useMemo(() => {
    if (!data) return false;
    return !(
      (presetId === "" ? null : presetId) === (data.preset_id ?? (defaultPresetId || null)) &&
      (isCustom ? customModel.trim() || null : null) === (data.custom_model_id ?? null) &&
      (isCustom ? customTokenizer.trim() || null : null) === (data.custom_tokenizer_id ?? null) &&
      (isCustom && customVectorSize ? Number.parseInt(customVectorSize, 10) || null : null) ===
        (data.custom_vector_size ?? null) &&
      (isCustom ? customDtype : null) === (isCustom ? (data.custom_dtype ?? "F16") : null) &&
      (isCustom && customMaxSeq ? Number.parseInt(customMaxSeq, 10) || null : null) ===
        (data.custom_max_sequence_length ?? null) &&
      (isCustom ? customMultimodal : null) ===
        (isCustom ? (data.custom_is_multimodal ?? false) : null)
    );
  }, [
    data,
    isCustom,
    presetId,
    customModel,
    customTokenizer,
    customVectorSize,
    customDtype,
    customMaxSeq,
    customMultimodal,
    defaultPresetId,
  ]);

  useEffect(() => {
    onDirtyChange(buildPayload(), edited);
  }, [buildPayload, edited, onDirtyChange]);

  // Whether saving will actually reset the vectordb. Mirrors the backend's
  // `needs_vectordb_reset` (model id OR vector dimension changes) so the
  // parent's destructive warning fires only for a real index reset, not for
  // a dtype / max-seq / tokenizer / multimodal-only edit. Compared against
  // the persisted effective runtime; only meaningful while the form is
  // actually edited (a clean form resets nothing). Note: the backend
  // additionally factors in `LOOKBACK_EMBEDDING_*` dev env overrides; this UI
  // prediction is preset/custom-only, which matches the production (no-env)
  // path the warning targets.
  const resetsVectordb = useMemo(() => {
    if (remote) return false;
    if (!edited || !effective) return false;
    const nextModelId = isCustom ? customModel.trim() : (selectedPreset?.hf_repo ?? "");
    const nextVectorSize = isCustom
      ? Number.parseInt(customVectorSize, 10) || 0
      : (selectedPreset?.vector_size ?? 0);
    return nextModelId !== effective.model_id || nextVectorSize !== effective.vector_size;
  }, [remote, edited, effective, isCustom, customModel, customVectorSize, selectedPreset]);
  useEffect(() => {
    onResetsVectordbChange?.(resetsVectordb);
  }, [resetsVectordb, onResetsVectordbChange]);

  return (
    <div className="settings-card">
      <div className="settings-card-title">{t("settings.embedding.title")}</div>
      <div className="settings-card-desc">
        {t(remote ? "settings.embedding.remoteDesc" : "settings.embedding.desc")}
      </div>
      <ModelStatusSection
        purpose={t("settings.embedding.modelPurpose")}
        status={status}
        retrying={retrying}
        onRetry={onRetry}
      />
      {/* Prominent destructive warning, shown ONLY when the pending change
          actually resets the vectordb (model id / vector dimension change).
          A dtype / max-seq / tokenizer / multimodal-only edit is saveable but
          leaves the index intact, so it gets a milder note instead — warning
          "the vectordb will be reset" there would be a false alarm. */}
      {resetsVectordb && !suppressResetWarning ? (
        <div className="settings-destructive-banner">{t("settings.embedding.resetBanner")}</div>
      ) : (
        buildPayload() !== null &&
        !remote &&
        !suppressResetWarning && (
          <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginBottom: 8 }}>
            {t("settings.embedding.noResetNote")}
          </div>
        )
      )}
      {remote && (
        <div
          style={{
            color: "var(--warning)",
            fontSize: 11,
            background: "var(--bg-secondary)",
            padding: 8,
            borderRadius: 4,
            marginBottom: 8,
          }}
        >
          {t("settings.embedding.remoteCaution")}
        </div>
      )}
      {effective && (
        <SettingRow
          label={t("settings.embedding.currentSettings")}
          value={`${effective.model_id} (${effective.vector_size} dim, ${effective.dtype}${
            effective.is_multimodal ? ", multimodal" : ""
          })`}
        />
      )}
      <div className="settings-row">
        <div className="settings-row-label">{t("settings.embedding.preset")}</div>
        <select
          value={presetId}
          disabled={disabled}
          onChange={(e) => setPresetId(e.target.value)}
          style={{ flex: 1 }}
        >
          {presetList.map((p) => (
            <option key={p.id} value={p.id}>
              {t("settings.embedding.presetOption", {
                name: p.display_name,
                ram: formatRamGb(p.estimated_ram_gb),
              })}
            </option>
          ))}
          <option value={CUSTOM_EMBEDDING_PRESET_ID}>{t("settings.embedding.customOption")}</option>
        </select>
      </div>
      {selectedPreset && !isCustom && (
        <div className="settings-preset-desc">
          <div>{t(selectedPreset.description)}</div>
          <div className="settings-ram-estimate">
            {t("settings.embedding.ramEstimate", {
              ram: formatRamGb(selectedPreset.estimated_ram_gb),
            })}
          </div>
        </div>
      )}
      {isCustom && (
        <>
          <div className="settings-row">
            <div className="settings-row-label">HF Repo</div>
            <input
              type="text"
              value={customModel}
              disabled={disabled}
              placeholder="org/name"
              onChange={(e) => setCustomModel(e.target.value)}
              style={{ flex: 1 }}
            />
          </div>
          <div className="settings-row">
            <div className="settings-row-label">{t("settings.embedding.tokenizer")}</div>
            <input
              type="text"
              value={customTokenizer}
              disabled={disabled}
              placeholder={t("settings.embedding.tokenizerPlaceholder")}
              onChange={(e) => setCustomTokenizer(e.target.value)}
              style={{ flex: 1 }}
            />
          </div>
          <div className="settings-row">
            <div className="settings-row-label">Vector size</div>
            <input
              type="number"
              value={customVectorSize}
              disabled={disabled}
              placeholder="e.g. 1024"
              onChange={(e) => setCustomVectorSize(e.target.value)}
              style={{ flex: 1 }}
            />
          </div>
          <div className="settings-row">
            <div className="settings-row-label">Dtype</div>
            <select
              value={customDtype}
              disabled={disabled}
              onChange={(e) => setCustomDtype(e.target.value)}
              style={{ flex: 1 }}
            >
              <option value="F16">F16</option>
              <option value="BF16">BF16</option>
              <option value="F32">F32</option>
            </select>
          </div>
          <div className="settings-row">
            <div className="settings-row-label">Max seq length</div>
            <input
              type="number"
              value={customMaxSeq}
              disabled={disabled}
              placeholder="e.g. 8192"
              onChange={(e) => setCustomMaxSeq(e.target.value)}
              style={{ flex: 1 }}
            />
          </div>
          <div className="settings-row">
            <div className="settings-row-label">{t("settings.embedding.multimodal")}</div>
            <label style={{ flex: 1, fontSize: 12 }}>
              <input
                type="checkbox"
                checked={customMultimodal}
                disabled={disabled}
                onChange={(e) => setCustomMultimodal(e.target.checked)}
              />{" "}
              {t("settings.embedding.multimodalLabel")}
            </label>
          </div>
        </>
      )}
      {/* The evacuate-vs-delete choice only matters when the save actually
          resets the vectordb; for a dtype/max-seq/tokenizer/multimodal-only
          edit the index is untouched, so hide it to avoid implying the index
          is at risk. */}
      {resetsVectordb && !suppressResetWarning && (
        <div className="settings-row">
          <div className="settings-row-label">{t("settings.embedding.existingVectordb")}</div>
          <label style={{ flex: 1, fontSize: 12 }}>
            <input
              type="checkbox"
              checked={evacuate}
              disabled={disabled}
              onChange={(e) => setEvacuate(e.target.checked)}
            />{" "}
            {t("settings.embedding.evacuateLabel")}
          </label>
        </div>
      )}
      {isCustom && !customRequiredOk && (
        <div style={{ color: "var(--danger)", fontSize: 11 }}>
          {t("settings.embedding.customRequired")}
        </div>
      )}
    </div>
  );
}

// Inclusive bounds for the MCP tool-call timeout, mirroring the backend's
// `mcp_settings::validate_set_request` (`1..=REQUEST_TIMEOUT_SEC_MAX`). Kept in
// sync by hand so the card rejects an out-of-range value up front instead of
// arming the save bar for a restart the backend will then reject.
const MCP_TIMEOUT_SEC_MIN = 1;
const MCP_TIMEOUT_SEC_MAX = 3600;

/**
 * Expose Lookback's RAG retrieval (`lookback_recall`) as MCP tools
 * so an external MCP client (e.g. Claude Desktop) can search the user's
 * memories. jobworkerp boots its in-process MCP HTTP server alongside the
 * gRPC front when `MCP_ENABLED=true`, narrowed to the `lookback-mcp-rag`
 * function-set via `MCP_SET_NAME`. The toggle is read at sidecar spawn time,
 * so saving restarts the sidecar (handled by the unified `apply_settings`).
 *
 * Works in both local and remote connection modes — the MCP server runs in the
 * local sidecar (always up), and the memories endpoint the exposed
 * `lookback_recall` searches follows the active connection.
 */
export function McpServerCard({
  onDirtyChange,
  resetSignal,
}: DirtyReporter<SetMcpSettingsRequest>) {
  const { t } = useTranslation();
  const { data } = useQuery({
    queryKey: ["mcp-settings"],
    queryFn: getMcpSettings,
  });

  const [enabled, setEnabled] = useState(false);
  const [showAdvanced, setShowAdvanced] = useState(false);
  // Advanced overrides. Empty string ⇒ "leave jobworkerp default" (null).
  const [streaming, setStreaming] = useState<"" | "true" | "false">("");
  const [timeoutSec, setTimeoutSec] = useState("");

  const seedFromData = useCallback(() => {
    if (!data) return;
    setEnabled(data.enabled);
    setStreaming(data.streaming == null ? "" : data.streaming ? "true" : "false");
    setTimeoutSec(data.request_timeout_sec != null ? String(data.request_timeout_sec) : "");
  }, [data]);

  useEffect(() => {
    seedFromData();
  }, [seedFromData]);
  // biome-ignore lint/correctness/useExhaustiveDependencies: resetSignal is the discard trigger
  useEffect(() => {
    if (resetSignal === 0) return;
    seedFromData();
  }, [resetSignal]);

  // Parse the timeout once and share it between the payload and the validity
  // gate. Empty ⇒ null (use the jobworkerp default); otherwise an integer in
  // `[MIN, MAX]` mirroring the backend's `validate_set_request` (rejects 0 /
  // floats / >MAX) so the save bar never arms for a restart the backend will
  // reject. `null` from a non-empty input means "invalid".
  const timeoutVal = useMemo<number | null>(() => {
    const raw = timeoutSec.trim();
    if (raw === "") return null;
    const n = Number(raw);
    return Number.isInteger(n) && n >= MCP_TIMEOUT_SEC_MIN && n <= MCP_TIMEOUT_SEC_MAX ? n : null;
  }, [timeoutSec]);
  const timeoutInvalid = timeoutSec.trim() !== "" && timeoutVal === null;

  // Build the payload mirroring the backend contract; null when the form
  // matches the persisted value (so the card is "clean").
  const buildPayload = useCallback((): SetMcpSettingsRequest | null => {
    if (!data) return null;
    const streamingVal: boolean | null = streaming === "" ? null : streaming === "true";
    const payload: SetMcpSettingsRequest = {
      enabled,
      // exclude_runner/worker are not surfaced in the UI yet (the set already
      // scopes the tools); pass through the persisted values verbatim.
      exclude_runner_as_tool: data.exclude_runner_as_tool,
      exclude_worker_as_tool: data.exclude_worker_as_tool,
      streaming: streamingVal,
      request_timeout_sec: timeoutVal,
    };
    const clean =
      payload.enabled === data.enabled &&
      payload.streaming === (data.streaming ?? null) &&
      payload.request_timeout_sec === (data.request_timeout_sec ?? null);
    return clean ? null : payload;
  }, [data, enabled, streaming, timeoutVal]);

  useEffect(() => {
    const built = buildPayload();
    // `edited` must be true whenever the form differs from the persisted
    // value — INCLUDING an invalid timeout, so the leave-guard still blocks an
    // unconfirmed navigation/discard. `buildPayload` alone can't see that: an
    // out-of-range timeout collapses `timeoutVal` to null, which then matches a
    // persisted-null and reports "clean". Compare the RAW timeout input against
    // the persisted string form to catch that case (mirrors how the LLM card's
    // invalid edits still build a non-null payload and thus stay "edited").
    const persistedTimeout =
      data?.request_timeout_sec != null ? String(data.request_timeout_sec) : "";
    const timeoutEdited = timeoutSec.trim() !== persistedTimeout;
    const edited = built !== null || timeoutEdited;
    // An invalid timeout is not saveable (payload null), but it is still
    // "edited" above so the guard fires.
    const payload = timeoutInvalid ? null : built;
    onDirtyChange(payload, edited);
  }, [buildPayload, data, timeoutSec, timeoutInvalid, onDirtyChange]);

  const activePort = data?.active_port ?? null;
  const setName = data?.set_name ?? "lookback-mcp-rag";
  const mcpUrl = activePort != null ? `http://127.0.0.1:${activePort}/mcp` : null;
  const clientConfigExample =
    mcpUrl != null ? JSON.stringify({ mcpServers: { lookback: { url: mcpUrl } } }, null, 2) : null;

  return (
    <div className="settings-card">
      <div className="settings-card-title">{t("settings.mcp.title")}</div>
      <div className="settings-card-desc">
        {t("settings.mcp.descPre")} (<code style={{ fontSize: 10 }}>lookback_recall</code>){" "}
        {t("settings.mcp.descMid")} <code style={{ fontSize: 10 }}>{setName}</code>{" "}
        {t("settings.mcp.descPost")}
      </div>

      <div className="settings-row">
        <div className="settings-row-label">{t("settings.mcp.server")}</div>
        <label style={{ display: "flex", alignItems: "center", gap: 6, fontSize: 12 }}>
          <input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.target.checked)} />
          {t("settings.mcp.enable")}
        </label>
      </div>

      {enabled && (
        <div className="settings-row">
          <div className="settings-row-label">{t("settings.mcp.url")}</div>
          {mcpUrl != null ? (
            <code style={{ fontSize: 11, userSelect: "all" }}>{mcpUrl}</code>
          ) : (
            <span style={{ fontSize: 11, color: "var(--label-tertiary)" }}>
              {t("settings.mcp.urlPending")}
            </span>
          )}
        </div>
      )}

      {enabled && clientConfigExample != null && (
        <>
          <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 4 }}>
            {t("settings.mcp.clientConfigExample")}
          </div>
          <pre
            style={{
              fontFamily: "var(--font-mono)",
              fontSize: 11,
              marginTop: 4,
              padding: 8,
              background: "var(--secondary-bg)",
              borderRadius: 6,
              userSelect: "all",
            }}
          >
            {clientConfigExample}
          </pre>
        </>
      )}

      <div className="settings-row">
        <div className="settings-row-label" />
        <button
          type="button"
          className="btn"
          onClick={() => setShowAdvanced((v) => !v)}
          style={{ fontSize: 11 }}
        >
          {showAdvanced
            ? t("settings.mcp.advancedToggleOpen")
            : t("settings.mcp.advancedToggleClosed")}
        </button>
      </div>
      {showAdvanced && (
        <>
          <div className="settings-row">
            <div className="settings-row-label">streaming</div>
            <select
              value={streaming}
              onChange={(e) => setStreaming(e.target.value as "" | "true" | "false")}
              style={{ width: 160 }}
            >
              <option value="">{t("settings.mcp.streamingDefault")}</option>
              <option value="true">{t("settings.mcp.streamingOn")}</option>
              <option value="false">{t("settings.mcp.streamingOff")}</option>
            </select>
          </div>
          <div className="settings-row">
            <div className="settings-row-label">{t("settings.mcp.timeout")}</div>
            <input
              type="number"
              value={timeoutSec}
              placeholder="60"
              min={MCP_TIMEOUT_SEC_MIN}
              max={MCP_TIMEOUT_SEC_MAX}
              onChange={(e) => setTimeoutSec(e.target.value)}
              style={{ width: 120 }}
            />
            <span style={{ marginLeft: 8, color: "var(--label-tertiary)", fontSize: 11 }}>
              {t("settings.mcp.timeoutHint")}
            </span>
          </div>
          {timeoutInvalid && (
            <div style={{ color: "var(--danger)", fontSize: 11 }}>
              {t("settings.mcp.timeoutInvalid", {
                min: MCP_TIMEOUT_SEC_MIN,
                max: MCP_TIMEOUT_SEC_MAX,
              })}
            </div>
          )}
        </>
      )}

      <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 4 }}>
        {t("settings.mcp.restartHint")}
      </div>
    </div>
  );
}

/**
 * Non-dismissable progress modal shown while a long-running save is in
 * flight. Blocks every other Settings interaction so the user can't
 * fire a second save / change tabs and lose the spinner context.
 *
 * Pinning `onClose` to a no-op disables click-outside / Escape; the
 * modal disappears only when the parent stops rendering it.
 */
function SavingBlockerModal({ title, description }: { title: string; description: string }) {
  return (
    <Modal dismissable={false} ariaLabel={title}>
      <div style={{ padding: 24, minWidth: 320 }}>
        <div style={{ fontSize: 14, fontWeight: 600, marginBottom: 12 }}>
          <span className="saving-spinner" />
          {title}
        </div>
        <div style={{ fontSize: 12, color: "var(--label-secondary)", lineHeight: 1.6 }}>
          {description}
        </div>
      </div>
    </Modal>
  );
}

const BACKGROUND_JOB_KIND_KEYS: Record<BackgroundTaskKind, string> = {
  embedding: "settings.backgroundJobs.kind.embedding",
  summary: "settings.backgroundJobs.kind.summary",
  personality: "settings.backgroundJobs.kind.personality",
  reflection: "settings.backgroundJobs.kind.reflection",
  llm_other: "settings.backgroundJobs.kind.llmOther",
};

export const BACKGROUND_JOB_QUEUE_REFETCH_INTERVAL_MS = 10_000;
export const BACKGROUND_JOB_QUEUE_IDLE_REFETCH_INTERVAL_MS = 60_000;

export function backgroundJobQueueRefetchInterval(active: boolean): number {
  return active
    ? BACKGROUND_JOB_QUEUE_REFETCH_INTERVAL_MS
    : BACKGROUND_JOB_QUEUE_IDLE_REFETCH_INTERVAL_MS;
}

export function BackgroundJobQueueCard() {
  const { t } = useTranslation();
  const jobs = useQuery({
    queryKey: ["background-job-queue-status"],
    queryFn: getBackgroundJobQueueStatus,
    refetchInterval: (query) =>
      backgroundJobQueueRefetchInterval(query.state.data?.active ?? false),
  });
  const rows = jobs.data?.rows ?? [];
  const count = (value: number) => (jobs.isLoading ? "…" : String(value));

  return (
    <div className="settings-card">
      <div className="settings-card-title">{t("settings.backgroundJobs.title")}</div>
      <div className="settings-card-desc">{t("settings.backgroundJobs.desc")}</div>
      {jobs.error && (
        <div style={{ color: "var(--danger)", fontSize: 11 }}>{(jobs.error as Error).message}</div>
      )}
      <div
        style={{
          display: "grid",
          gridTemplateColumns: "minmax(90px, 1fr) repeat(4, auto)",
          gap: "4px 10px",
          fontSize: 11,
          marginTop: 8,
        }}
      >
        <span />
        <span>{t("settings.backgroundJobs.pending")}</span>
        <span>{t("settings.backgroundJobs.running")}</span>
        <span>{t("settings.backgroundJobs.waitResult")}</span>
        <span>{t("settings.backgroundJobs.cancelling")}</span>
        {rows.map((row) => (
          <div key={row.kind} style={{ display: "contents" }}>
            <span>{t(BACKGROUND_JOB_KIND_KEYS[row.kind])}</span>
            <span>{count(row.pending)}</span>
            <span>{count(row.running)}</span>
            <span>{count(row.wait_result)}</span>
            <span>{count(row.cancelling)}</span>
          </div>
        ))}
      </div>
      <div className="settings-row" style={{ marginTop: 8 }}>
        <div style={{ color: "var(--label-tertiary)", fontSize: 11 }}>
          {t("settings.backgroundJobs.delayHint")}
        </div>
        <button
          type="button"
          className="btn"
          onClick={() => void jobs.refetch()}
          disabled={jobs.isFetching}
        >
          {jobs.isFetching ? t("common.loading") : t("common.reload")}
        </button>
      </div>
    </div>
  );
}

export function MemoryEmbeddingCard() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const stats = useQuery({
    queryKey: ["memory-embedding-stats"],
    queryFn: getMemoryEmbeddingStats,
  });
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<RedispatchEmbeddingsResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  const {
    total_records: total = 0,
    records_with_embedding: withCount = 0,
    records_without_embedding: without = 0,
    vector_dimension: vectorDim = 0,
  } = stats.data ?? {};
  // vector_dimension === 0 means the memory_vector LanceDB table was never
  // created — the sidecar isn't up or MEMORY_VECTOR_ENABLED is unset.
  // Disable the re-dispatch button in that case because dispatching against
  // a missing table would fail at upsert time.
  const tableMissing = vectorDim === 0;
  const fmtCount = (n: number) => (stats.isLoading ? "…" : String(n));

  const handleRedispatch = async () => {
    // Even with zero un-embedded rows the action is sometimes desired
    // (e.g. embedding model swap), so the confirm names both the cost and
    // the typical use case rather than gating on `without > 0`.
    if (!window.confirm(t("settings.embeddingIndex.redispatchConfirm", { count: total }))) {
      return;
    }
    setBusy(true);
    setError(null);
    setResult(null);
    try {
      const res = await redispatchMemoryEmbeddings({});
      setResult(res);
      await queryClient.invalidateQueries({ queryKey: ["memory-embedding-stats"] });
      await queryClient.invalidateQueries({ queryKey: ["background-job-queue-status"] });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="settings-card">
      <div className="settings-card-title">{t("settings.embeddingIndex.title")}</div>
      <div className="settings-card-desc">{t("settings.embeddingIndex.desc")}</div>
      {stats.error && (
        <div style={{ color: "var(--danger)", fontSize: 11 }}>{(stats.error as Error).message}</div>
      )}
      {tableMissing && !stats.isLoading && !stats.error && (
        <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginBottom: 4 }}>
          {t("settings.embeddingIndex.tableMissing")}
        </div>
      )}
      <SettingRow label={t("settings.embeddingIndex.total")} value={fmtCount(total)} />
      <SettingRow label={t("settings.embeddingIndex.embedded")} value={fmtCount(withCount)} />
      <SettingRow label={t("settings.embeddingIndex.pending")} value={fmtCount(without)} />
      {!stats.isLoading && total > 0 && without === 0 && (
        <div style={{ color: "var(--label-tertiary)", fontSize: 11 }}>
          {t("settings.embeddingIndex.allDone")}
        </div>
      )}
      <div className="settings-row">
        <div className="settings-row-label">{t("settings.embeddingIndex.redispatch")}</div>
        <button
          type="button"
          className="btn"
          onClick={() => void handleRedispatch()}
          disabled={busy || tableMissing}
          title={t("settings.embeddingIndex.redispatchTitle")}
        >
          {busy
            ? t("settings.embeddingIndex.redispatching")
            : t("settings.embeddingIndex.redispatchButton")}
        </button>
      </div>
      {result && (
        <div style={{ color: "var(--label-secondary)", fontSize: 11 }}>
          {t("settings.embeddingIndex.result", {
            dispatched: result.dispatched_count,
            skipped: result.skipped_count,
            failed: result.failed_count,
            ms: result.duration_ms,
          })}
        </div>
      )}
      {error && <div style={{ color: "var(--danger)", fontSize: 11 }}>{error}</div>}
    </div>
  );
}

// Tauri ダイアログでディレクトリを選ばせる薄いラッパ。dialog permission は
// `tauri.conf.json` で有効化済み。HfHomeCard / DataRootCard の両方が同一
// オプションで呼ぶので一箇所に集約しておく。ユーザがキャンセルしたら null。
async function pickDirectory(): Promise<string | null> {
  const picked = await openDialog({ directory: true, multiple: false });
  return typeof picked === "string" ? picked : null;
}

const HF_HOME_OPTIONS: { value: HfHomeMode; labelKey: string; helpKey: string }[] = [
  {
    value: "global",
    labelKey: "settings.hfHome.modeGlobal",
    helpKey: "settings.hfHome.helpGlobal",
  },
  {
    value: "data_root",
    labelKey: "settings.hfHome.modeDataRoot",
    helpKey: "settings.hfHome.helpDataRoot",
  },
  {
    value: "custom",
    labelKey: "settings.hfHome.modeCustom",
    helpKey: "settings.hfHome.helpCustom",
  },
];

/**
 * HF_HOME (HuggingFace モデルキャッシュの場所) の選択 UI。保存すると sidecar
 * を再起動して新しいパスを反映する。Models カードのスキャン対象も同じ
 * 解決値 (effective_hf_home) を見ているので、切替後すぐに「準備完了/未ダウンロード」
 * の表示が更新される。
 */
type HfHomeCardProps = DirtyReporter<SetHfHomeRequest> & {
  previewDataRoot?: string;
};

function modelsPathUnder(root: string): string {
  const trimmed = root.trim();
  if (!trimmed) return "";
  return trimmed.endsWith("/") ? `${trimmed}models` : `${trimmed}/models`;
}

export function HfHomeCard({ onDirtyChange, resetSignal, previewDataRoot }: HfHomeCardProps) {
  const { t } = useTranslation();
  const { data } = useQuery({
    queryKey: ["app-settings"],
    queryFn: getAppSettings,
  });

  // Matches `AppSettings::default()` on the Rust side so the segment
  // control isn't briefly highlighting the wrong option between mount
  // and the `getAppSettings` seed effect.
  const [mode, setMode] = useState<HfHomeMode>("global");
  const [path, setPath] = useState("");

  const seedFromData = useCallback(() => {
    if (!data) return;
    setMode(data.hf_home_mode);
    setPath(data.hf_home_path ?? "");
  }, [data]);

  useEffect(() => {
    seedFromData();
  }, [seedFromData]);
  // biome-ignore lint/correctness/useExhaustiveDependencies: resetSignal is the discard trigger
  useEffect(() => {
    if (resetSignal === 0) return;
    seedFromData();
  }, [resetSignal]);

  const handlePick = async () => {
    const picked = await pickDirectory();
    if (picked) setPath(picked);
  };

  // `data` not loaded yet → not dirty.
  const dirty =
    !!data &&
    (mode !== data.hf_home_mode ||
      (mode === "custom" && path.trim() !== (data.hf_home_path ?? "")));
  const customIncomplete = mode === "custom" && path.trim() === "";
  const activeOption = HF_HOME_OPTIONS.find((o) => o.value === mode);
  const previewRoot = previewDataRoot?.trim() || data?.resolved.current_data_root || "";
  const previewEffectiveHfHome =
    mode === "data_root"
      ? modelsPathUnder(previewRoot)
      : mode === "custom"
        ? path.trim() || "—"
        : (data?.resolved.effective_hf_home ?? "—");
  const previewDiffers =
    !!data?.resolved.effective_hf_home &&
    previewEffectiveHfHome !== data.resolved.effective_hf_home;

  // Report (payload, edited). `dirty` already means "differs from
  // persisted" → that IS edited (guards navigation even when the custom
  // path is incomplete). The saveable payload is null while incomplete.
  useEffect(() => {
    const payload =
      dirty && !customIncomplete
        ? { mode, path: mode === "custom" ? path.trim() || null : null }
        : null;
    onDirtyChange(payload, dirty);
  }, [dirty, customIncomplete, mode, path, onDirtyChange]);

  return (
    <div className="settings-card">
      <div className="settings-card-title">{t("settings.hfHome.title")}</div>
      <div className="settings-card-desc">{t("settings.hfHome.desc")}</div>
      <div className="settings-row">
        <div className="settings-row-label">{t("settings.hfHome.location")}</div>
        <div className="segment">
          {HF_HOME_OPTIONS.map((opt) => (
            <button
              key={opt.value}
              type="button"
              className={`segment-btn${mode === opt.value ? " active" : ""}`}
              onClick={() => setMode(opt.value)}
            >
              {t(opt.labelKey)}
            </button>
          ))}
        </div>
      </div>
      {activeOption && (
        <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
          {t(activeOption.helpKey)}
        </div>
      )}
      {mode === "custom" && (
        <div className="settings-row">
          <div className="settings-row-label">{t("settings.hfHome.path")}</div>
          <input
            type="text"
            value={path}
            placeholder="/Volumes/Ext/hf"
            onChange={(e) => setPath(e.target.value)}
            style={{ flex: 1 }}
          />
          <button
            type="button"
            className="btn"
            onClick={() => void handlePick()}
            style={{ marginLeft: 8 }}
          >
            {t("settings.hfHome.pick")}
          </button>
        </div>
      )}
      <SettingRow
        label={
          previewDiffers
            ? t("settings.hfHome.effectivePathAfter")
            : t("settings.hfHome.effectivePathCurrent")
        }
        value={previewEffectiveHfHome}
      />
      {customIncomplete && (
        <div style={{ color: "var(--danger)", fontSize: 11 }}>
          {t("settings.hfHome.customIncomplete")}
        </div>
      )}
    </div>
  );
}

/**
 * App data dir (Lookback のアプリケーションデータルート) の選択 UI。
 * 変更は bootstrap.json に保存され、次回アプリ起動から有効になる。
 * sqlite / LanceDB / tonic channel が現在のルートに紐付いて生きているため
 * ランタイム切替はせず、保存後にアプリ再起動を促す。
 */
export function DataRootCard({
  sqlitePath,
  lancedbPath,
  pluginsPath,
  logPath,
  onEditedChange,
}: {
  sqlitePath: string | null;
  lancedbPath: string | null;
  pluginsPath: string | null;
  logPath: string | null;
  /** Reports whether the App data dir field differs from the persisted
   *  value, so the parent can arm its leave-guards. Saves itself (not via the
   *  unified save bar), so only the edited flag is lifted — no payload. */
  onEditedChange?: (edited: boolean) => void;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { data, refetch } = useQuery({
    queryKey: ["app-settings"],
    queryFn: getAppSettings,
  });

  const [path, setPath] = useState("");
  const [validation, setValidation] = useState<DataRootValidation | null>(null);
  const [saving, setSaving] = useState(false);
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [savedMsg, setSavedMsg] = useState<string | null>(null);

  useEffect(() => {
    if (!data) return;
    setPath(data.data_root_override ?? "");
  }, [data]);

  const trimmedPath = path.trim();

  // 入力デバウンス & validate。空欄は OK 扱いにし (= デフォルトに戻す保存)、
  // 何か入力されている場合のみ Rust 側に検証を投げる。
  useEffect(() => {
    if (!trimmedPath) {
      setValidation(null);
      return;
    }
    const handle = window.setTimeout(() => {
      validateDataRoot(trimmedPath)
        .then(setValidation)
        .catch(() => setValidation(null));
    }, 300);
    return () => window.clearTimeout(handle);
  }, [trimmedPath]);

  const handlePick = async () => {
    const picked = await pickDirectory();
    if (picked) setPath(picked);
  };

  // 「作成」ボタン: validation.creatable のときだけ表示される。mkdir -p
  // 成功直後はディレクトリが新規 (= writable, Lookback ルートではない)
  // と分かっているので、もう一度 validateDataRoot を invoke せず ok=true
  // を直接セットして Save ボタンを解放する。これで Tauri IPC ラウンド
  // トリップと余分な write probe を 1 回ずつ節約。
  const handleCreate = async () => {
    setCreating(true);
    setError(null);
    try {
      await createDataRoot(trimmedPath);
      setValidation({
        ok: true,
        writable: true,
        is_existing_lookback_root: false,
        creatable: false,
        message: null,
      });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setCreating(false);
    }
  };

  const handleSave = async () => {
    const next = trimmedPath === "" ? null : trimmedPath;
    const confirmMsg = next
      ? t("settings.dataRoot.saveConfirmChange")
      : t("settings.dataRoot.saveConfirmReset");
    if (!window.confirm(confirmMsg)) {
      return;
    }
    setSaving(true);
    setError(null);
    setSavedMsg(null);
    try {
      await setDataRoot(next);
      await Promise.all([refetch(), queryClient.invalidateQueries({ queryKey: ["settings"] })]);
      setSavedMsg(
        next
          ? t("settings.dataRoot.savedMsgChange", { path: next })
          : t("settings.dataRoot.savedMsgReset"),
      );
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setSaving(false);
    }
  };

  const dirty = (data?.data_root_override ?? "") !== trimmedPath;
  const saveDisabled =
    saving || !dirty || (trimmedPath !== "" && validation !== null && !validation.ok);

  // Lift the dirty flag so the parent can arm its leave-guards; clear it on
  // unmount (view switch / tab change) so a stale "edited" doesn't keep the
  // guard armed after the card is gone.
  useEffect(() => {
    onEditedChange?.(dirty);
  }, [dirty, onEditedChange]);
  useEffect(() => {
    return () => onEditedChange?.(false);
  }, [onEditedChange]);

  return (
    <div className="settings-card">
      <div className="settings-card-title">{t("settings.dataRoot.title")}</div>
      <div className="settings-card-desc">
        {t("settings.dataRoot.desc", {
          defaultRoot: data?.resolved.default_data_root ?? "—",
        })}
      </div>
      <div className="settings-row">
        <div className="settings-row-label">App data dir</div>
        <input
          type="text"
          value={path}
          placeholder={data?.resolved.default_data_root ?? "—"}
          onChange={(e) => setPath(e.target.value)}
          style={{ flex: 1 }}
        />
        <button
          type="button"
          className="btn"
          onClick={() => void handlePick()}
          style={{ marginLeft: 8 }}
        >
          {t("settings.dataRoot.pick")}
        </button>
      </div>
      {validation?.creatable && (
        <div className="settings-row" style={{ marginTop: 2 }}>
          <div className="settings-row-label" />
          <button
            type="button"
            className="btn"
            onClick={() => void handleCreate()}
            disabled={creating}
            title={t("settings.dataRoot.createTitle")}
            style={{ fontSize: 11 }}
          >
            {creating ? t("settings.dataRoot.creating") : t("settings.dataRoot.create")}
          </button>
        </div>
      )}
      {validation?.message && (
        <div
          style={{
            color: validation.ok ? "var(--label-tertiary)" : "var(--danger)",
            fontSize: 11,
            marginTop: 2,
          }}
        >
          {t(validation.message)}
        </div>
      )}
      <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
        {t("settings.dataRoot.currentRunning")}{" "}
        <code style={{ fontSize: 10 }}>{data?.resolved.current_data_root ?? "—"}</code>
        {data && data.resolved.current_data_root !== data.resolved.pending_data_root && (
          <>
            <br />
            {t("settings.dataRoot.nextLaunch")}{" "}
            <strong style={{ color: "var(--label-secondary)" }}>
              {data.resolved.pending_data_root}
            </strong>{" "}
            {t("settings.dataRoot.nextLaunchNote")}
          </>
        )}
      </div>
      <SettingRow label="SQLite" value={sqlitePath ?? "—"} />
      <SettingRow label="LanceDB" value={lancedbPath ?? "—"} />
      <SettingRow label="Plugins" value={pluginsPath ?? "—"} />
      <SettingRow label="Logs" value={logPath ?? "—"} />
      <div className="settings-row">
        <div className="settings-row-label" />
        <button
          type="button"
          className="btn primary"
          onClick={() => void handleSave()}
          disabled={saveDisabled}
        >
          {saving ? t("settings.dataRoot.saving") : t("settings.dataRoot.save")}
        </button>
        {savedMsg && (
          <span style={{ color: "var(--success, #2ea043)", fontSize: 11, marginLeft: 8 }}>
            {savedMsg}
          </span>
        )}
      </div>
      {error && <div style={{ color: "var(--danger)", fontSize: 11 }}>{error}</div>}
    </div>
  );
}
