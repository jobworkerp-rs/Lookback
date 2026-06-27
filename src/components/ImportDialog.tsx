import { open as openDirectoryDialog } from "@tauri-apps/plugin-dialog";
import { useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { startImport } from "@/api";
import { hasLlmInitFailure, type SidecarStatus } from "@/hooks/useSidecarStatus";
import { localDateToIsoUtc, localTodayMinusDays } from "@/lib/dateInput";
import {
  isValidPlainSourceName,
  loadPlainThreadStrategy,
  savePlainThreadStrategy,
} from "@/lib/plainImport";
import type { ImportSource, PlainImportConfig, ThreadStrategy } from "@/types/api";
import { DateInput } from "./DateInput";
import { Modal } from "./Modal";

export interface ImportDialogProps {
  open: boolean;
  onClose: () => void;
  onStarted: (jobId: string) => void;
  /** Absolute path to the `memories-import` binary. Empty when resolution failed. */
  memoriesImportBin: string;
  /** Error from `resolve_memories_import_bin`, if any. Blocks submission. */
  resolveError: string | null;
  /**
   * Sidecar status from the parent. Pulled through props rather than
   * re-subscribed with `useSidecarStatus()` here: the dialog mounts
   * after `sidecar://ready` already fired, so a fresh listener would
   * never receive the warnings and the LLM-init banner would stay
   * silent.
   */
  sidecar: SidecarStatus;
}

export function ImportDialog({
  open,
  onClose,
  onStarted,
  memoriesImportBin,
  resolveError,
  sidecar,
}: ImportDialogProps) {
  const { t } = useTranslation();
  const [claude, setClaude] = useState(true);
  const [codex, setCodex] = useState(true);
  // Plain source is off by default (it needs an explicit directory). The
  // grouping strategy seeds from the last choice (persisted in localStorage).
  const [plain, setPlain] = useState(false);
  const [plainRoot, setPlainRoot] = useState("");
  const [plainSourceName, setPlainSourceName] = useState("");
  const [plainStrategy, setPlainStrategy] = useState<ThreadStrategy>(() =>
    loadPlainThreadStrategy(),
  );
  const [sinceMode, setSinceMode] = useState<"all" | "from">("from");
  const [sinceDate, setSinceDate] = useState(() => localTodayMinusDays(1));
  const [labels, setLabels] = useState("");
  const [dryRun, setDryRun] = useState(false);
  // Post-import generation toggles default to on so the dialog reproduces
  // the previous "run everything" behaviour unless the user opts out.
  const [runSummary, setRunSummary] = useState(true);
  const [runPersonality, setRunPersonality] = useState(true);
  const [runReflection, setRunReflection] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const llmDown = hasLlmInitFailure(sidecar);
  // These steps require the LLM; disable them (and never queue them) when the
  // plugin failed to init or when dry-run skips dispatch entirely.
  const genDisabled = llmDown || dryRun;
  // Effective checkbox value: masked off while generation is disabled so the
  // visual state and the wire value can't diverge.
  const effective = (v: boolean) => v && !genDisabled;

  if (!open) return null;

  const choosePlainRoot = async () => {
    const selected = await openDirectoryDialog({ directory: true, multiple: false });
    if (typeof selected === "string") setPlainRoot(selected);
  };

  const onStrategyChange = (v: ThreadStrategy) => {
    setPlainStrategy(v);
    savePlainThreadStrategy(v);
  };

  const handleSubmit = async () => {
    setError(null);
    const sources: ImportSource[] = [];
    if (claude) sources.push("claude-code");
    if (codex) sources.push("codex");
    if (plain) sources.push("plain");

    if (sources.length === 0) {
      setError(t("import.errorNoSource"));
      return;
    }

    let plainConfig: PlainImportConfig | undefined;
    if (plain) {
      if (plainRoot.trim() === "") {
        setError(t("import.errorNoPlainRoot"));
        return;
      }
      const name = plainSourceName.trim();
      if (name !== "" && !isValidPlainSourceName(name)) {
        setError(t("import.errorInvalidSourceName"));
        return;
      }
      plainConfig = {
        root: plainRoot,
        source_name: name === "" ? undefined : name,
        thread_strategy: plainStrategy,
      };
    }

    let since: string | undefined;
    if (sinceMode === "from") {
      since = localDateToIsoUtc(sinceDate);
      // Guard the from-mode regression: a cleared/invalid date must not
      // silently fall through to a full (all-history) import.
      if (since === undefined) {
        setError(t("import.errorInvalidDate"));
        return;
      }
    }

    const labelList = labels
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);

    setBusy(true);
    try {
      // Dispatch id doubles as the cancel key: the toast's Stop button
      // sends it to start_import_cancel which targets the same in-flight
      // map entry the Rust side registers from this request. The
      // backend echoes the same id back as `res.job_id` so the
      // existing event correlation by job_id continues to work.
      const dispatch_id = crypto.randomUUID();
      const res = await startImport({
        sources,
        since,
        user_id: 1,
        dry_run: dryRun,
        labels: labelList,
        memories_import_bin: memoriesImportBin,
        // Mask so a down LLM (or dry-run) never queues a step that can only
        // fail; the server also ignores these on dry-run.
        run_summary: effective(runSummary),
        run_personality: effective(runPersonality),
        run_reflection: effective(runReflection),
        dispatch_id,
        plain: plainConfig,
      });
      onStarted(res.job_id);
      onClose();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal onClose={onClose} ariaLabel={t("import.title")}>
      <div className="modal-head">
        <div className="modal-title">{t("import.title")}</div>
      </div>
      <div className="modal-body">
        {llmDown && (
          <div className="warning-banner">
            <div className="warning-banner-title">{t("import.llmDownTitle")}</div>
            <div className="warning-banner-body">
              <Trans
                i18nKey="import.llmDownBody"
                components={{ code1: <code />, code2: <code /> }}
                values={{
                  pluginPath: "<repo>/plugins/",
                  dylib: "libjobworkerp_llama_cpp_plugin.dylib",
                }}
              />
            </div>
          </div>
        )}
        <div className="field">
          <span className="field-label">{t("import.targetType")}</span>
          <label className="checkbox-row">
            <input type="checkbox" checked={claude} onChange={(e) => setClaude(e.target.checked)} />
            claude-code (~/.claude/projects)
          </label>
          <label className="checkbox-row">
            <input type="checkbox" checked={codex} onChange={(e) => setCodex(e.target.checked)} />
            codex (~/.codex/sessions)
          </label>
          <label className="checkbox-row">
            <input type="checkbox" checked={plain} onChange={(e) => setPlain(e.target.checked)} />
            {t("import.targetPlain")}
          </label>
          {plain && (
            <div className="field plain-options">
              <span className="field-label">{t("import.plainRoot")}</span>
              <div className="plain-root-picker">
                <button type="button" className="btn" onClick={choosePlainRoot}>
                  {t("import.plainChoose")}
                </button>
                <span className="plain-root-path">{plainRoot || "—"}</span>
              </div>
              <span className="field-label">{t("import.plainSourceName")}</span>
              <input
                type="text"
                className="text-input"
                value={plainSourceName}
                onChange={(e) => setPlainSourceName(e.target.value)}
                placeholder="obsidian-private"
              />
              <span className="field-label">{t("import.plainStrategy")}</span>
              <select
                className="text-input"
                value={plainStrategy}
                onChange={(e) => onStrategyChange(e.target.value as ThreadStrategy)}
              >
                <option value="per-file">{t("import.plainStrategyPerFile")}</option>
                <option value="per-dir">{t("import.plainStrategyPerDir")}</option>
                <option value="single">{t("import.plainStrategySingle")}</option>
              </select>
            </div>
          )}
        </div>

        <div className="field">
          <span className="field-label">{t("import.period")}</span>
          <div className="radio-row">
            <label className="checkbox-row">
              <input
                type="radio"
                checked={sinceMode === "all"}
                onChange={() => setSinceMode("all")}
              />
              {t("import.periodAll")}
            </label>
            <label className="checkbox-row">
              <input
                type="radio"
                checked={sinceMode === "from"}
                onChange={() => setSinceMode("from")}
              />
              {t("import.periodFrom")}
            </label>
            <DateInput
              value={sinceDate}
              onChange={setSinceDate}
              disabled={sinceMode === "all"}
              className="" // sits inside a radio-row, not styled as a standalone .text-input
            />
          </div>
        </div>

        <div className="field">
          <span className="field-label">{t("import.labels")}</span>
          <input
            type="text"
            className="text-input"
            value={labels}
            onChange={(e) => setLabels(e.target.value)}
            placeholder="refactor, debugging"
          />
        </div>

        <div className="field">
          <label className="checkbox-row">
            <input type="checkbox" checked={dryRun} onChange={(e) => setDryRun(e.target.checked)} />
            {t("import.dryRun")}
          </label>
        </div>

        <div className="field">
          <span className="field-label">{t("import.postGen")}</span>
          <label className="checkbox-row">
            <input
              type="checkbox"
              checked={effective(runSummary)}
              disabled={genDisabled}
              onChange={(e) => setRunSummary(e.target.checked)}
            />
            {t("import.runSummary")}
          </label>
          <label className="checkbox-row">
            <input
              type="checkbox"
              checked={effective(runPersonality)}
              disabled={genDisabled}
              onChange={(e) => setRunPersonality(e.target.checked)}
            />
            {t("import.runPersonality")}
          </label>
          <label className="checkbox-row">
            <input
              type="checkbox"
              checked={effective(runReflection)}
              disabled={genDisabled}
              onChange={(e) => setRunReflection(e.target.checked)}
            />
            {t("import.runReflection")}
          </label>
        </div>

        {resolveError && (
          <div style={{ color: "var(--danger)", fontSize: 12, marginTop: 8 }}>{resolveError}</div>
        )}
        {error && <div style={{ color: "var(--danger)", fontSize: 12, marginTop: 8 }}>{error}</div>}
      </div>
      <div className="modal-foot">
        <button type="button" className="btn" onClick={onClose} disabled={busy}>
          {t("common.cancel")}
        </button>
        <button
          type="button"
          className="btn primary"
          onClick={handleSubmit}
          disabled={busy || !memoriesImportBin}
        >
          {busy ? t("import.starting") : t("import.start")}
        </button>
      </div>
    </Modal>
  );
}
