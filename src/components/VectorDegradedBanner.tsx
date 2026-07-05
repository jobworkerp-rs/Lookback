import { useTranslation } from "react-i18next";
import type { VectorDegradedInfo } from "@/hooks/useSidecarStatus";

/**
 * Persistent, non-fatal banner shown at the top of the main region while the
 * local vector store is degraded (a dimension mismatch forced the memories
 * sidecar to restart with vectors disabled). Unlike `BootError`, the app is
 * fully browsable — this only tells the user that embedding-dependent
 * features are off and offers a shortcut into the embedding settings card.
 *
 * `info.expectedDim` / `actualDim` come from the sidecar's degraded warning
 * detail; when the detail couldn't be parsed we still render the banner with
 * the dimension-free copy (degraded is true regardless).
 */
export function VectorDegradedBanner({
  info,
  onOpenEmbeddingSettings,
}: {
  info: VectorDegradedInfo;
  onOpenEmbeddingSettings: () => void;
}) {
  const { t } = useTranslation();
  const hasDims = info.expectedDim != null && info.actualDim != null;
  return (
    <div className="warning-banner" role="alert">
      <div className="warning-banner-title">{t("sidecar.degraded.title")}</div>
      <div className="warning-banner-body">
        {hasDims
          ? t("sidecar.degraded.bodyWithDims", {
              expectedDim: info.expectedDim,
              actualDim: info.actualDim,
            })
          : t("sidecar.degraded.body")}
      </div>
      <button type="button" className="warning-banner-cta" onClick={onOpenEmbeddingSettings}>
        {t("sidecar.degraded.cta")}
      </button>
    </div>
  );
}
