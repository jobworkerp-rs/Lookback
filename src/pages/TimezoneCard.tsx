import { useQuery } from "@tanstack/react-query";
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { getAppSettings, listTimezones } from "@/api";
import type { SetTimezoneRequest } from "@/types/api";
import type { DirtyReporter } from "./Settings";

type TimezoneCardProps = DirtyReporter<SetTimezoneRequest> & { disabled?: boolean };

/** Sentinel select value for "Auto (follow OS)". Empty string maps to a
 *  `null` timezone request (clear the explicit selection). */
const TIMEZONE_AUTO = "";

function settingPayload(zone: string, dirty: boolean): SetTimezoneRequest | null {
  if (!dirty) return null;
  const normalized = zone.trim();
  return { timezone: normalized === TIMEZONE_AUTO ? null : normalized };
}

function missingPersistedZone(zone: string, options: string[]): string | null {
  return zone !== TIMEZONE_AUTO && !options.includes(zone) ? zone : null;
}

function SettingRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="setting-row">
      <div>{label}</div>
      <code>{value}</code>
    </div>
  );
}

/**
 * Workflow timezone selector. Persists an explicit IANA zone into
 * app-settings.json (or `null` for "Auto"), which the sidecar injects as the
 * jobworkerp worker's `TZ` for the summary/import day/week/month boundary jq.
 * `TZ` is spawn-time env, so saving restarts the sidecar (unified save bar).
 * Disabled in remote connection mode because those workflows run on the remote
 * jobworkerp, where this local app-settings file cannot change the worker TZ.
 */
export function TimezoneCard({ onDirtyChange, resetSignal, disabled = false }: TimezoneCardProps) {
  const { t } = useTranslation();
  const { data } = useQuery({
    queryKey: ["app-settings"],
    queryFn: getAppSettings,
  });
  const { data: zones } = useQuery({
    queryKey: ["timezones"],
    queryFn: listTimezones,
    staleTime: Number.POSITIVE_INFINITY,
  });

  const [zone, setZone] = useState<string>(TIMEZONE_AUTO);

  const seedFromData = useCallback(() => {
    if (!data) return;
    setZone(data.timezone ?? TIMEZONE_AUTO);
  }, [data]);

  useEffect(() => {
    seedFromData();
  }, [seedFromData]);
  // biome-ignore lint/correctness/useExhaustiveDependencies: resetSignal is the discard trigger
  useEffect(() => {
    if (resetSignal === 0) return;
    seedFromData();
  }, [resetSignal]);

  const dirty = !disabled && !!data && zone !== (data.timezone ?? TIMEZONE_AUTO);

  // `dirty` already folds in `!disabled`, so `settingPayload` returns `null`
  // whenever the card is disabled — no separate `disabled` guard needed.
  useEffect(() => {
    onDirtyChange(settingPayload(zone, dirty), dirty);
  }, [dirty, zone, onDirtyChange]);

  // The persisted zone may be a value the host tzdb no longer lists (e.g. a
  // config synced from another machine); keep it selectable so the user isn't
  // silently switched to Auto.
  const options = zones ?? [];
  const hasTimezoneList = options.length > 0;
  const persistedMissing = missingPersistedZone(zone, options);

  return (
    <div className="settings-card">
      <div className="settings-card-title">{t("settings.timezone.title")}</div>
      <div className="settings-card-desc">{t("settings.timezone.desc")}</div>
      <div className="settings-row">
        <div className="settings-row-label">{t("settings.timezone.label")}</div>
        {hasTimezoneList ? (
          <select
            value={zone}
            onChange={(e) => setZone(e.target.value)}
            style={{ flex: 1 }}
            disabled={disabled}
          >
            <option value={TIMEZONE_AUTO}>{t("settings.timezone.auto")}</option>
            {persistedMissing && <option value={persistedMissing}>{persistedMissing}</option>}
            {options.map((z) => (
              <option key={z} value={z}>
                {z}
              </option>
            ))}
          </select>
        ) : (
          <input
            type="text"
            value={zone}
            placeholder="Asia/Tokyo"
            onChange={(e) => setZone(e.target.value)}
            style={{ flex: 1 }}
            disabled={disabled}
          />
        )}
      </div>
      {disabled && (
        <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
          {t("settings.timezone.remoteDisabled")}
        </div>
      )}
      {!hasTimezoneList && (
        <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
          {t("settings.timezone.manualHelp")}
        </div>
      )}
      {zone.trim() === TIMEZONE_AUTO && (
        <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 }}>
          {t("settings.timezone.autoHelp")}
        </div>
      )}
      <SettingRow
        label={t("settings.timezone.effectiveCurrent")}
        value={data?.effective_timezone ?? "\u2014"}
      />
      {dirty && (
        <div style={{ color: "var(--label-tertiary)", fontSize: 11 }}>
          {t("settings.timezone.restartHint")}
        </div>
      )}
    </div>
  );
}
