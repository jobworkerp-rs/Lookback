import { useQuery } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { getAppSettings } from "@/api";
import { SettingsCheckboxRow } from "@/components/SettingsCheckboxRow";
import type { MigrationBackupSettings } from "@/types/api";
import type { DirtyReporter } from "./Settings";

/** Upper bound for kept backups; each backup is a full database copy, so a
 *  larger count is almost certainly a typo rather than an intent. */
const MAX_KEPT_BACKUPS = 100;

function parseKeep(raw: string): number | null {
  if (!/^\d+$/.test(raw.trim())) return null;
  const keep = Number(raw);
  return keep <= MAX_KEPT_BACKUPS ? keep : null;
}

/**
 * Backup choice handed to the memories migration tool at startup. The tool
 * backs up only when a migration will change the database and prunes old
 * backups itself; this card only records whether to back up and how many to
 * keep. Turning the backup off is allowed for low-disk machines but leaves a
 * failed migration with no restore point.
 */
export function MigrationBackupCard({
  onDirtyChange,
  resetSignal,
}: DirtyReporter<MigrationBackupSettings>) {
  const { t } = useTranslation();
  const { data } = useQuery({ queryKey: ["app-settings"], queryFn: getAppSettings });
  const saved = data?.migration_backup;
  const [enabled, setEnabled] = useState(true);
  const [keepInput, setKeepInput] = useState("1");

  const seedFromData = useCallback(() => {
    if (!saved) return;
    setEnabled(saved.enabled);
    setKeepInput(String(saved.keep));
  }, [saved]);

  useEffect(() => {
    seedFromData();
  }, [seedFromData]);

  useEffect(() => {
    if (resetSignal === 0) return;
    seedFromData();
  }, [resetSignal, seedFromData]);

  const keep = parseKeep(keepInput);
  const edited =
    saved !== undefined && (enabled !== saved.enabled || keepInput !== String(saved.keep));
  const payload = useMemo<MigrationBackupSettings | null>(
    () => (edited && keep !== null ? { enabled, keep } : null),
    [edited, enabled, keep],
  );

  useEffect(() => {
    onDirtyChange(payload, edited);
  }, [onDirtyChange, payload, edited]);

  return (
    <section className="settings-card" aria-label={t("settings.migrationBackup.title")}>
      <div className="settings-card-title">{t("settings.migrationBackup.title")}</div>
      <div className="settings-card-desc">{t("settings.migrationBackup.desc")}</div>
      <SettingsCheckboxRow
        id="migration-backup-enabled"
        checked={enabled}
        disabled={saved === undefined}
        onCheckedChange={setEnabled}
        title={t("settings.migrationBackup.enabled")}
        description={t("settings.migrationBackup.enabledDescription")}
      />
      {!enabled && (
        <div className="form-error" role="alert">
          {t("settings.migrationBackup.disabledWarning")}
        </div>
      )}
      <div className="settings-row">
        <label className="settings-row-label" htmlFor="migration-backup-keep">
          {t("settings.migrationBackup.keep")}
        </label>
        <input
          id="migration-backup-keep"
          type="number"
          min={0}
          max={MAX_KEPT_BACKUPS}
          step={1}
          value={keepInput}
          disabled={saved === undefined || !enabled}
          onChange={(event) => setKeepInput(event.target.value)}
          style={{ width: 84 }}
        />
      </div>
      <div style={{ color: "var(--label-tertiary)", fontSize: 11, marginTop: 4 }}>
        {t("settings.migrationBackup.keepDescription")}
      </div>
      {keep === null && (
        <div className="form-error" role="status">
          {t("settings.migrationBackup.keepInvalid", { max: MAX_KEPT_BACKUPS })}
        </div>
      )}
    </section>
  );
}
