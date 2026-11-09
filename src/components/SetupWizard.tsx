import { open } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  applySetup,
  getConnectionConfig,
  planEmbeddingChange,
  restartForSetup,
  resumeSetup,
  switchEmbeddingSpace,
  validateDataRoot,
} from "@/api";
import { EmbeddingProviderCard, HfHomeCard, LlmProviderCard } from "@/pages/Settings";
import type {
  DataRootValidation,
  EmbeddingBackup,
  EmbeddingMigrationPlan,
  SetEmbeddingSettingsRequest,
  SetHfHomeRequest,
  SetLlmSettingsRequest,
} from "@/types/api";
import { Modal } from "./Modal";

// The wizard is a linear state machine over these steps. Naming them keeps the
// transitions (setStep, step === …) readable and localises the step count.
const STEP = {
  welcome: 0,
  dataRoot: 1,
  embedding: 2,
  llm: 3,
  apply: 4,
} as const;
const TOTAL_STEPS = Object.keys(STEP).length;

interface SetupWizardProps {
  resumeApply: boolean;
  currentDataRoot: string;
  defaultDataRoot: string;
  onComplete: () => void;
}

export function SetupWizard({
  resumeApply,
  currentDataRoot,
  defaultDataRoot,
  onComplete,
}: SetupWizardProps) {
  const { t, i18n } = useTranslation();
  const preferredLanguage = i18n.resolvedLanguage === "ja" ? "ja" : "en";
  const [step, setStep] = useState<number>(resumeApply ? STEP.apply : STEP.welcome);
  const [dataRoot, setDataRoot] = useState("");
  const [dataRootValidation, setDataRootValidation] = useState<DataRootValidation | null>(null);
  const [llm, setLlm] = useState<SetLlmSettingsRequest | null>(null);
  const [embedding, setEmbedding] = useState<SetEmbeddingSettingsRequest | null>(null);
  const [hfHome, setHfHome] = useState<SetHfHomeRequest | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [dataRootDialogError, setDataRootDialogError] = useState<string | null>(null);
  const [restartRequired, setRestartRequired] = useState(false);
  const [embeddingPlan, setEmbeddingPlan] = useState<EmbeddingMigrationPlan | null>(null);
  const [embeddingPlanSettings, setEmbeddingPlanSettings] =
    useState<SetEmbeddingSettingsRequest | null>(null);
  const [embeddingBackupDirectory, setEmbeddingBackupDirectory] = useState("");
  const [embeddingNoBackupUnsafe, setEmbeddingNoBackupUnsafe] = useState(false);
  const [embeddingMaintenanceAck, setEmbeddingMaintenanceAck] = useState(false);
  const [embeddingSwitchCompleted, setEmbeddingSwitchCompleted] = useState(false);
  const applyStarted = useRef(false);

  useEffect(() => {
    const value = dataRoot.trim();
    if (!value) {
      setDataRootValidation(null);
      return;
    }
    const timer = window.setTimeout(() => {
      validateDataRoot(value)
        .then(setDataRootValidation)
        .catch((reason) =>
          setDataRootValidation({
            ok: false,
            writable: false,
            is_existing_lookback_root: false,
            creatable: false,
            message: String(reason),
          }),
        );
    }, 300);
    return () => window.clearTimeout(timer);
  }, [dataRoot]);

  const applySelectedSettings = useCallback(
    async (embeddingSettings: SetEmbeddingSettingsRequest | null) =>
      applySetup({
        data_root: dataRoot.trim() || null,
        preferred_language: preferredLanguage,
        settings: {
          llm,
          embedding: embeddingSettings,
          hf_home: hfHome,
          mcp: null,
          timezone: null,
          issue_tracker: null,
          migration_backup: null,
        },
      }),
    [dataRoot, hfHome, llm, preferredLanguage],
  );

  const runApply = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      if (resumeApply) {
        await resumeSetup();
        onComplete();
        return;
      }
      if (embedding && !embeddingSwitchCompleted) {
        const connection = await getConnectionConfig();
        if (connection.memories.mode === "local") {
          const planned = await planEmbeddingChange(embedding);
          if (planned.kind === "failed") {
            throw new Error(
              `${planned.data.command} (${planned.data.stage}: ${planned.data.error_code})`,
            );
          }
          if (planned.kind !== "plan") {
            throw new Error(`Unexpected embedding plan result: ${planned.kind}`);
          }
          if (planned.data.decision === "reembed_required") {
            if (!planned.data.executable) {
              throw new Error(planned.data.reason ?? t("settings.embeddingPlan.manualRecovery"));
            }
            setEmbeddingPlan(planned.data);
            setEmbeddingPlanSettings(embedding);
            setEmbeddingBackupDirectory("");
            setEmbeddingNoBackupUnsafe(false);
            setEmbeddingMaintenanceAck(false);
            return;
          }
        }
      }
      const response = await applySelectedSettings(embeddingSwitchCompleted ? null : embedding);
      if (response.restart_required) {
        setRestartRequired(true);
      } else {
        onComplete();
      }
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
      applyStarted.current = false;
    } finally {
      setBusy(false);
    }
  }, [applySelectedSettings, embedding, embeddingSwitchCompleted, onComplete, resumeApply, t]);

  useEffect(() => {
    if (step !== STEP.apply || applyStarted.current) return;
    applyStarted.current = true;
    void runApply();
  }, [runApply, step]);

  const chooseDirectory = async () => {
    setDataRootDialogError(null);
    try {
      const selected = await open({ directory: true, multiple: false });
      if (typeof selected === "string") setDataRoot(selected);
    } catch (reason) {
      setDataRootDialogError(reason instanceof Error ? reason.message : String(reason));
    }
  };

  const retry = () => {
    applyStarted.current = true;
    void runApply();
  };

  const chooseEmbeddingBackupDirectory = async () => {
    setError(null);
    try {
      const selected = await open({ directory: true, multiple: false });
      if (typeof selected === "string") {
        setEmbeddingBackupDirectory(selected);
        setEmbeddingNoBackupUnsafe(false);
      }
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    }
  };

  const approveEmbeddingMigration = async () => {
    if (!embeddingPlan || !embeddingPlanSettings) return;
    if ((!embeddingBackupDirectory && !embeddingNoBackupUnsafe) || !embeddingMaintenanceAck) return;
    const backup: EmbeddingBackup = embeddingBackupDirectory
      ? { mode: "directory", path: embeddingBackupDirectory }
      : { mode: "no_backup_unsafe" };

    setBusy(true);
    setError(null);
    let switched = false;
    try {
      const result = await switchEmbeddingSpace({
        settings: embeddingPlanSettings,
        expected_space: embeddingPlan.current_space,
        backup,
      });
      if (result.result.kind !== "switched") {
        if (result.result.kind === "failed") {
          throw new Error(
            `${result.result.data.command} (${result.result.data.stage}: ${result.result.data.error_code})`,
          );
        }
        throw new Error(`Embedding migration did not switch (${result.result.kind})`);
      }
      switched = true;
      setEmbeddingSwitchCompleted(true);
      setEmbeddingPlan(null);
      setEmbeddingPlanSettings(null);

      // The approved switch owns the embedding settings; the setup batch only
      // applies the remaining selected settings and cannot repeat the change.
      const response = await applySelectedSettings(null);
      if (response.restart_required) {
        setRestartRequired(true);
      } else {
        onComplete();
      }
    } catch (reason) {
      setEmbeddingPlan(null);
      setEmbeddingPlanSettings(null);
      if (!switched) setEmbeddingSwitchCompleted(false);
      setError(reason instanceof Error ? reason.message : String(reason));
      applyStarted.current = false;
    } finally {
      setBusy(false);
    }
  };

  const cancelEmbeddingMigration = () => {
    setEmbeddingPlan(null);
    setEmbeddingPlanSettings(null);
    setError(null);
    applyStarted.current = false;
    setStep(STEP.embedding);
  };

  const updateEmbedding = (payload: SetEmbeddingSettingsRequest | null) => {
    setEmbedding(payload);
    setEmbeddingSwitchCompleted(false);
  };

  const dataRootInvalid =
    dataRoot.trim() !== "" &&
    (dataRootValidation === null || (!dataRootValidation.ok && !dataRootValidation.creatable));

  const skipStep = () => {
    if (step === STEP.dataRoot) {
      setDataRoot("");
      setHfHome(null);
    } else if (step === STEP.embedding) {
      updateEmbedding(null);
    } else if (step === STEP.llm) {
      setLlm(null);
    }
    setStep((value) => value + 1);
  };

  return (
    <Modal wide dismissable={false} ariaLabel={t("setup.title")}>
      <div className="modal-head">
        <div className="modal-title">{t("setup.title")}</div>
        <div className="setup-step-counter">
          {step + 1} / {TOTAL_STEPS}
        </div>
      </div>
      <div className="modal-body setup-wizard-body">
        {step === STEP.welcome && (
          <div className="settings-card">
            <div className="settings-card-title">{t("setup.welcome.title")}</div>
            <p>{t("setup.welcome.intro")}</p>
            <p>{t("setup.welcome.note")}</p>
          </div>
        )}
        {step === STEP.dataRoot && (
          <>
            <div className="settings-card">
              <div className="settings-card-title">{t("setup.dataRoot.title")}</div>
              <div className="settings-card-desc">
                {t("setup.dataRoot.desc")} <code>{currentDataRoot}</code>
              </div>
              <div className="settings-row">
                <input
                  value={dataRoot}
                  placeholder={defaultDataRoot}
                  onChange={(event) => setDataRoot(event.target.value)}
                />
                <button type="button" className="btn" onClick={() => void chooseDirectory()}>
                  {t("setup.dataRoot.choose")}
                </button>
              </div>
              {dataRootValidation?.message && (
                <div className={dataRootValidation.ok ? undefined : "setup-field-error"}>
                  {t(dataRootValidation.message)}
                </div>
              )}
              {dataRootDialogError && (
                <div className="setup-field-error">
                  {t("setup.dataRoot.dialogError", { error: dataRootDialogError })}
                </div>
              )}
            </div>
            <HfHomeCard
              onDirtyChange={setHfHome}
              previewDataRoot={dataRoot.trim() || currentDataRoot}
              resetSignal={0}
            />
          </>
        )}
        {step === STEP.embedding && (
          <EmbeddingProviderCard
            retrying={false}
            onRetry={() => {}}
            onDirtyChange={updateEmbedding}
            preferredDefaultLanguage={preferredLanguage}
            suppressResetWarning
            resetSignal={0}
          />
        )}
        {step === STEP.llm && (
          <LlmProviderCard
            retrying={false}
            onRetry={() => {}}
            onDirtyChange={setLlm}
            pendingEmbeddingSettings={embedding}
            resetSignal={0}
          />
        )}
        {step === STEP.apply &&
          (embeddingPlan && embeddingPlanSettings ? (
            <div className="settings-card">
              <div className="settings-card-title">{t("settings.embeddingPlan.title")}</div>
              <p>{t("settings.embeddingPlan.description")}</p>
              <p>
                {t("settings.embeddingPlan.counts", {
                  memoryText: embeddingPlan.memory_text,
                  memoryMedia: embeddingPlan.memory_media,
                  thread: embeddingPlan.thread,
                  reflectionIntent: embeddingPlan.reflection_intent,
                })}
              </p>
              <p>
                {t("settings.embeddingPlan.failures", {
                  failedTransient:
                    embeddingPlan.failed_transient ?? t("settings.embeddingMigration.unknown"),
                  failedPermanent:
                    embeddingPlan.failed_permanent ?? t("settings.embeddingMigration.unknown"),
                })}
              </p>
              <div style={{ marginTop: 12, fontWeight: 600 }}>
                {t("settings.embeddingPlan.backupTitle")}
              </div>
              <div className="settings-row">
                <div className="settings-row-label">
                  {t("settings.embeddingPlan.backupDirectory")}
                </div>
                <input
                  value={embeddingBackupDirectory}
                  readOnly
                  aria-label={t("settings.embeddingPlan.backupDirectory")}
                  style={{ flex: 1 }}
                />
                <button
                  type="button"
                  className="btn"
                  disabled={busy}
                  onClick={() => void chooseEmbeddingBackupDirectory()}
                >
                  {t("settings.embeddingPlan.chooseDirectory")}
                </button>
              </div>
              <label className="settings-row" style={{ justifyContent: "flex-start" }}>
                <input
                  type="checkbox"
                  checked={embeddingNoBackupUnsafe}
                  disabled={busy || Boolean(embeddingBackupDirectory)}
                  onChange={(event) => setEmbeddingNoBackupUnsafe(event.target.checked)}
                />
                <span>{t("settings.embeddingPlan.noBackupUnsafe")}</span>
              </label>
              {!embeddingBackupDirectory && !embeddingNoBackupUnsafe && (
                <div role="alert" style={{ color: "var(--warning)" }}>
                  {t("settings.embeddingPlan.requiresBackup")}
                </div>
              )}
              <label className="settings-row" style={{ justifyContent: "flex-start" }}>
                <input
                  type="checkbox"
                  checked={embeddingMaintenanceAck}
                  disabled={busy}
                  onChange={(event) => setEmbeddingMaintenanceAck(event.target.checked)}
                />
                <span>{t("settings.embeddingPlan.maintenanceAck")}</span>
              </label>
              {error && (
                <div role="alert" style={{ color: "var(--danger)" }}>
                  {error}
                </div>
              )}
              <div className="settings-row">
                <button
                  type="button"
                  className="btn"
                  disabled={busy}
                  onClick={cancelEmbeddingMigration}
                >
                  {t("settings.embeddingPlan.cancel")}
                </button>
                <button
                  type="button"
                  className="btn danger"
                  disabled={
                    busy ||
                    (!embeddingBackupDirectory && !embeddingNoBackupUnsafe) ||
                    !embeddingMaintenanceAck
                  }
                  onClick={() => void approveEmbeddingMigration()}
                >
                  {busy
                    ? t("settings.embeddingPlan.switching")
                    : t("settings.embeddingPlan.switch")}
                </button>
              </div>
            </div>
          ) : (
            <div className="settings-card">
              <div className="settings-card-title">
                {restartRequired
                  ? t("setup.apply.restartTitle")
                  : busy
                    ? t("setup.apply.preparingTitle")
                    : t("setup.apply.resultTitle")}
              </div>
              {busy && (
                <div className="settings-row">
                  <span className="saving-spinner" aria-hidden="true" />
                  <span>{t("setup.apply.preparingDesc")}</span>
                </div>
              )}
              {restartRequired && (
                <>
                  <p>{t("setup.apply.restartDesc")}</p>
                  <button
                    type="button"
                    className="btn primary"
                    onClick={() => void restartForSetup()}
                  >
                    {t("setup.apply.restartButton")}
                  </button>
                </>
              )}
              {error && (
                <>
                  <div className="setup-field-error">{error}</div>
                  <div className="settings-row">
                    {!embeddingSwitchCompleted && (
                      <button type="button" className="btn" onClick={() => setStep(STEP.llm)}>
                        {t("setup.apply.backToFix")}
                      </button>
                    )}
                    <button type="button" className="btn primary" onClick={retry}>
                      {t("setup.apply.retry")}
                    </button>
                  </div>
                </>
              )}
            </div>
          ))}
      </div>
      {step < STEP.apply && (
        <div className="modal-foot setup-wizard-foot">
          {step > STEP.welcome ? (
            <button type="button" className="btn" onClick={() => setStep((value) => value - 1)}>
              {t("setup.nav.back")}
            </button>
          ) : (
            <button type="button" className="btn" onClick={() => setStep(STEP.apply)}>
              {t("setup.nav.skipAndStart")}
            </button>
          )}
          <div className="setup-wizard-foot-spacer" />
          {step > STEP.welcome && (
            <button type="button" className="btn" onClick={skipStep}>
              {t("setup.nav.skip")}
            </button>
          )}
          <button
            type="button"
            className="btn primary"
            disabled={step === STEP.dataRoot && dataRootInvalid}
            onClick={() => setStep((value) => value + 1)}
          >
            {step === STEP.welcome ? t("setup.nav.start") : t("setup.nav.next")}
          </button>
        </div>
      )}
    </Modal>
  );
}
