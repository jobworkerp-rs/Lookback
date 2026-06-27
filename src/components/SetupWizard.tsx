import { open } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { applySetup, restartForSetup, resumeSetup, validateDataRoot } from "@/api";
import { EmbeddingProviderCard, HfHomeCard, LlmProviderCard } from "@/pages/Settings";
import type {
  DataRootValidation,
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
  const { t } = useTranslation();
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

  const runApply = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      if (resumeApply) {
        await resumeSetup();
        onComplete();
        return;
      }
      const response = await applySetup({
        data_root: dataRoot.trim() || null,
        settings: { llm, embedding, hf_home: hfHome, mcp: null },
      });
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
  }, [dataRoot, embedding, hfHome, llm, onComplete, resumeApply]);

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

  const dataRootInvalid =
    dataRoot.trim() !== "" &&
    (dataRootValidation === null || (!dataRootValidation.ok && !dataRootValidation.creatable));

  const skipStep = () => {
    if (step === STEP.dataRoot) {
      setDataRoot("");
      setHfHome(null);
    } else if (step === STEP.embedding) {
      setEmbedding(null);
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
            onDirtyChange={setEmbedding}
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
        {step === STEP.apply && (
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
                  <button type="button" className="btn" onClick={() => setStep(STEP.llm)}>
                    {t("setup.apply.backToFix")}
                  </button>
                  <button type="button" className="btn primary" onClick={retry}>
                    {t("setup.apply.retry")}
                  </button>
                </div>
              </>
            )}
          </div>
        )}
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
