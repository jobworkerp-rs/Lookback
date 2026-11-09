import { useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { getConnectionConfig, getEmbeddingRebuildProgress, getEmbeddingSpace } from "@/api";
import { rejectionReasonLabel, splitWriteRejections } from "@/lib/embeddingWriteRejections";
import type {
  EmbeddingMigrationFailedEvent,
  EmbeddingRebuildProgressView,
  EmbeddingSpaceView,
} from "@/types/api";

function migrationMarkers(space: EmbeddingSpaceView | undefined) {
  return (space?.tables ?? [])
    .flatMap((table) => {
      const marker = table.record?.migration_marker;
      return marker ? [{ table: table.table, ...marker }] : [];
    })
    .filter(
      (marker, index, all) =>
        all.findIndex((candidate) => candidate.attempt_id === marker.attempt_id) === index,
    );
}

function failedCount(kind: EmbeddingRebuildProgressView["kinds"][number]): number {
  return kind.failed_transient + kind.failed_permanent + kind.failed_to_dispatch;
}

/** Non-blocking shared notice for pages that run embedding-powered search. */
export function EmbeddingSpaceStatus({
  readyEpoch = 0,
  failureEvent = null,
}: {
  readyEpoch?: number;
  failureEvent?: EmbeddingMigrationFailedEvent | null;
}) {
  const { t } = useTranslation();
  const [dismissedNoticeKey, setDismissedNoticeKey] = useState<string | null>(null);
  const connection = useQuery({
    queryKey: ["connection-config"],
    queryFn: getConnectionConfig,
  });
  const config = connection.data;
  // The embedding space lives in memories, so only its target keys the cache.
  const remote = config?.memories.mode === "remote";
  const selectedTarget = remote ? (config.memories.remote_url ?? "") : "local";
  const space = useQuery<EmbeddingSpaceView>({
    queryKey: ["embedding-space", selectedTarget, readyEpoch],
    queryFn: getEmbeddingSpace,
    enabled: config != null,
    refetchInterval: config ? 5000 : false,
    // Retain the last observation if a transient refresh fails.
    retry: false,
  });
  const markers = migrationMarkers(space.data);
  const activeAttempt = markers.find((marker) => marker.state !== "complete")?.attempt_id ?? null;
  const hasPendingMarker = markers.some((marker) => marker.state !== "complete");
  const rebuild = useQuery({
    queryKey: ["embedding-rebuild-progress", selectedTarget, readyEpoch, activeAttempt],
    queryFn: () => getEmbeddingRebuildProgress(activeAttempt ?? ""),
    enabled: !remote && activeAttempt != null && space.data?.rpc_migration_supported === true,
    refetchInterval: activeAttempt ? 3000 : false,
    retry: false,
  });
  const matchingRebuild =
    activeAttempt && rebuild.data?.attempt_id === activeAttempt ? rebuild.data : null;

  // Discarded stale results are listed on the settings page instead.
  const rejections = splitWriteRejections(space.data?.write_rejections).warnings;
  const isPending = space.data?.registration_state === "pending";
  const hasNotice =
    failureEvent != null ||
    markers.length > 0 ||
    rejections.length > 0 ||
    connection.isError ||
    space.isError;
  const noticeKey = JSON.stringify({
    selectedTarget,
    readyEpoch,
    markers: markers
      .map(({ attempt_id, state }) => [attempt_id, state] as const)
      .sort(([left], [right]) => left.localeCompare(right)),
    registrationState: space.data?.registration_state,
    writeRejectionKinds: rejections.map(([name]) => name).sort(),
    failure: failureEvent
      ? {
          detail: failureEvent.detail,
          inspectRequired: failureEvent.inspect_required,
          stage: failureEvent.failure?.stage,
          errorCode: failureEvent.failure?.error_code,
          attempt: failureEvent.failure?.attempt?.attempt,
        }
      : null,
  });

  useEffect(() => {
    if (!hasNotice) setDismissedNoticeKey(null);
  }, [hasNotice]);

  if (connection.isPending && !config) return null;
  if (!config && !connection.isError) return null;
  if (!hasNotice || dismissedNoticeKey === noticeKey) return null;

  return (
    <section
      className="settings-card"
      aria-label={t("settings.embeddingSpaceStatus.title")}
      role="status"
      style={{ borderColor: "var(--warning)", padding: "8px 12px" }}
    >
      <div style={{ display: "flex", justifyContent: "flex-end" }}>
        <button
          type="button"
          aria-label={t("settings.embeddingSpaceStatus.dismiss")}
          onClick={() => setDismissedNoticeKey(noticeKey)}
          style={{ background: "transparent", border: 0, cursor: "pointer", padding: "0 4px" }}
        >
          <span aria-hidden="true">×</span>
        </button>
      </div>
      {!config && connection.isError && (
        <div role="alert" style={{ color: "var(--warning)", fontSize: 12 }}>
          {t("settings.embeddingSpaceStatus.refreshError", {
            error: connection.error ? String(connection.error) : t("common.unknown"),
          })}
        </div>
      )}
      {failureEvent && (
        <div role="alert" style={{ color: "var(--warning)", fontSize: 12 }}>
          {failureEvent.detail}
          {failureEvent.inspect_required && ` — ${t("settings.embeddingMigration.reload")}`}
        </div>
      )}
      {config && connection.isError && (
        <div role="alert" style={{ color: "var(--warning)", fontSize: 12 }}>
          {t("settings.embeddingSpaceStatus.refreshError", {
            error: String(connection.error),
          })}
        </div>
      )}
      {space.isError && (
        <div role="alert" style={{ color: "var(--warning)", fontSize: 12 }}>
          {t("settings.embeddingSpaceStatus.refreshError", { error: String(space.error) })}
        </div>
      )}
      {space.data && (
        <>
          {isPending && (
            <div style={{ color: "var(--warning)", fontSize: 12 }}>
              {t("settings.embeddingSpaceStatus.registrationPending")}
              <div>{t("settings.embeddingSpaceStatus.searchAvailable")}</div>
            </div>
          )}
          {markers.map((marker) => (
            <div key={marker.attempt_id} style={{ color: "var(--warning)", fontSize: 12 }}>
              {t(
                remote
                  ? "settings.embeddingSpaceStatus.remoteMarker"
                  : "settings.embeddingSpaceStatus.marker",
                {
                  state: marker.state,
                  attempt: marker.attempt_id,
                },
              )}
              {marker.state !== "complete" && (
                <>
                  <div>{t("settings.embeddingSpaceStatus.searchAvailable")}</div>
                  <div>{t("settings.embeddingSpaceStatus.partialResults")}</div>
                </>
              )}
            </div>
          ))}
          {remote && hasPendingMarker && (
            <div style={{ color: "var(--warning)", fontSize: 12 }}>
              {t("settings.embeddingSpaceStatus.remoteWarning")}
              <div>{t("settings.embeddingSpaceStatus.progressUnavailable")}</div>
            </div>
          )}
          {!remote && activeAttempt && !space.data.rpc_migration_supported && (
            <div style={{ color: "var(--warning)", fontSize: 12 }}>
              {t("settings.embeddingSpaceStatus.localProgressUnavailable")}
            </div>
          )}
          {matchingRebuild?.kinds.map((kind) => (
            <div key={`${matchingRebuild.attempt_id}:${kind.kind}`} style={{ fontSize: 11 }}>
              {t("settings.embeddingSpaceStatus.rebuild", {
                kind: kind.kind,
                complete: kind.complete,
                targets: kind.targets,
                dispatched: kind.dispatched,
                failed: failedCount(kind),
                failedToDispatch: kind.failed_to_dispatch,
              })}
            </div>
          ))}
          {matchingRebuild?.completion_expected && (
            <div role="status" style={{ fontSize: 11 }}>
              {t("settings.embeddingSpaceStatus.completionExpected")}
            </div>
          )}
          {activeAttempt && rebuild.data && rebuild.data.attempt_id !== activeAttempt && (
            <div role="alert" style={{ color: "var(--warning)", fontSize: 11 }}>
              {t("settings.embeddingMigration.staleProgress")}
            </div>
          )}
          {rebuild.isError && (
            <div role="alert" style={{ color: "var(--warning)", fontSize: 11 }}>
              {t("settings.embeddingSpaceStatus.progressError", { error: String(rebuild.error) })}
            </div>
          )}
          {rejections.map(([name, count]) => (
            <div key={name} style={{ color: "var(--warning)", fontSize: 11 }}>
              {t("settings.embeddingSpaceStatus.rejections", {
                reason: rejectionReasonLabel(t, name),
                count,
              })}
            </div>
          ))}
        </>
      )}
    </section>
  );
}
