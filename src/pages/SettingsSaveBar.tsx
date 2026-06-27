/**
 * Sticky bottom bar that aggregates the unsaved changes of the
 * restart-bearing Settings cards (LLM / Embedding / HF_HOME) into a single
 * save action, so the whole batch restarts the sidecar exactly once.
 *
 * Hidden entirely when nothing is dirty. When the pending embedding change
 * actually changes the model id / vector dimension, a prominent destructive
 * warning is shown because the vectordb will be reset (evacuated or deleted)
 * on save. A dtype / max-seq / tokenizer / multimodal-only embedding edit is
 * saveable but leaves the index intact, so it must NOT raise this warning.
 */
export interface SettingsSaveBarProps {
  /** Number of dirty restart-bearing cards (0 ⇒ the bar is not rendered). */
  dirtyCount: number;
  /** Whether saving will actually reset the vectordb (model id / vector
   *  dimension change). Drives the destructive warning — false for a
   *  dtype/max-seq/tokenizer/multimodal-only embedding edit. */
  resetsVectordb: boolean;
  saving: boolean;
  onDiscard: () => void;
  onSave: () => void;
}

import { useTranslation } from "react-i18next";

export function SettingsSaveBar({
  dirtyCount,
  resetsVectordb,
  saving,
  onDiscard,
  onSave,
}: SettingsSaveBarProps) {
  const { t } = useTranslation();
  if (dirtyCount === 0) return null;
  return (
    <section className="settings-save-bar" aria-label={t("settingsSaveBar.aria")}>
      <div className="settings-save-bar-text">
        <span className="settings-save-bar-count">
          {t("settingsSaveBar.count", { count: dirtyCount })}
        </span>
        {/* Every LLM / embedding / HF_HOME change restarts the sidecar (the
            backend may hot-reload some External-only swaps in place, but the
            UI can't prove which, so it always warns about a restart). */}
        <span className="settings-save-bar-hint">{t("settingsSaveBar.restartHint")}</span>
        {resetsVectordb && (
          <span className="settings-save-bar-warning">{t("settingsSaveBar.resetWarning")}</span>
        )}
      </div>
      <div className="settings-save-bar-actions">
        <button type="button" className="btn" onClick={onDiscard} disabled={saving}>
          {t("settingsSaveBar.discard")}
        </button>
        <button type="button" className="btn primary" onClick={onSave} disabled={saving}>
          {saving ? t("settingsSaveBar.saving") : t("settingsSaveBar.save")}
        </button>
      </div>
    </section>
  );
}
