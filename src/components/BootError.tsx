import { open as openDirectoryDialog } from "@tauri-apps/plugin-dialog";
import { open } from "@tauri-apps/plugin-shell";
import { type ReactNode, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  openLogDir,
  quitApp,
  recoverResetEmbeddingSettings,
  restoreDatabaseMigrationBackup,
  retryDatabaseMigration,
  startFreshSetup,
} from "@/api";
import { EmbeddingMigrationPanel } from "@/components/EmbeddingMigrationPanel";
import type {
  DatabaseRestoreResult,
  RecoveryResult,
  SidecarErrorPayload,
  StartupFailure,
  StartupFailureCode,
} from "@/types/api";

/** Stable per-action identifier. Used as React key, as the busy-state
 * key so only the clicked button shows a spinner (the others stay
 * idle-labelled but disabled), and as the discriminator for the
 * per-step progress messages. */
type ActionId =
  | "reset_embedding"
  | "retry_migration"
  | "restore_migration"
  | "open_migration_release"
  | "start_fresh_setup"
  | "open_log"
  | "quit";

export const MEMORY_KIND_MIGRATION_RELEASE_URL =
  "https://github.com/jobworkerp-rs/Lookback/releases/tag/v0.0.7";

/** Long-running recovery: invokes a Rust command that stops + restarts
 * the sidecar. `pendingLabelKey` is mandatory because the restart can take
 * 10–20 s, so the UI MUST surface a progress chip during the wait.
 * `run` resolves with a structured `RecoveryResult` the click handler
 * inspects for `restarted` / `restartError`.
 *
 * Labels are stored as i18n keys (not literals) and resolved with `t()`
 * at render time so the table can stay a module-level constant while
 * staying language-reactive. */
interface RecoveryRestartAction {
  kind: "restart";
  id: ActionId;
  labelKey: string;
  intent?: "primary" | "danger";
  pendingLabelKey: string;
  run: () => Promise<RecoveryResult>;
}

interface DatabaseRestoreAction {
  kind: "restore";
  id: "restore_migration";
  labelKey: string;
  intent: "danger";
  pendingLabelKey: string;
  run: () => Promise<DatabaseRestoreResult>;
}

/** Escape-hatch action that returns near-instantly (open Finder, exit
 * the app). No `pendingLabelKey` because there is nothing to wait on. */
interface RecoveryEscapeAction {
  kind: "escape";
  id: ActionId;
  labelKey: string;
  intent?: "primary" | "danger";
  run: () => Promise<void>;
}

type RecoveryAction = RecoveryRestartAction | DatabaseRestoreAction | RecoveryEscapeAction;

const resetEmbeddingAction: RecoveryRestartAction = {
  kind: "restart",
  id: "reset_embedding",
  labelKey: "bootError.action.resetEmbedding.label",
  pendingLabelKey: "bootError.action.resetEmbedding.pending",
  run: recoverResetEmbeddingSettings,
};
const retryMigrationAction: RecoveryRestartAction = {
  kind: "restart",
  id: "retry_migration",
  labelKey: "bootError.action.retryMigration.label",
  intent: "primary",
  pendingLabelKey: "bootError.action.retryMigration.pending",
  run: retryDatabaseMigration,
};
const openLogAction: RecoveryEscapeAction = {
  kind: "escape",
  id: "open_log",
  labelKey: "bootError.action.openLog.label",
  run: openLogDir,
};
const openMemoryKindMigrationReleaseAction: RecoveryEscapeAction = {
  kind: "escape",
  id: "open_migration_release",
  labelKey: "bootError.action.openMemoryKindMigrationRelease.label",
  intent: "primary",
  run: () => open(MEMORY_KIND_MIGRATION_RELEASE_URL),
};
/** Switches to a user-picked empty data root and restarts into setup. The
 * old root is left untouched, so this is the way out when a failed migration
 * cannot be restored. Cancelling the picker changes nothing. */
const startFreshSetupAction: RecoveryEscapeAction = {
  kind: "escape",
  id: "start_fresh_setup",
  labelKey: "bootError.action.startFreshSetup.label",
  intent: "danger",
  run: async () => {
    const picked = await openDirectoryDialog({ directory: true, multiple: false });
    if (typeof picked === "string") {
      await startFreshSetup(picked);
    }
  },
};
const quitAction: RecoveryEscapeAction = {
  kind: "escape",
  id: "quit",
  labelKey: "bootError.action.quit.label",
  run: quitApp,
};

/** `error_code` vocabulary of `memories-db-migrate local apply`, plus the
 * two codes Lookback reports itself when the tool cannot be started or ends
 * without its result line. The tool's specification owns the meaning of each
 * code; this table only decides what the user can do about it. */
export const MIGRATION_ERROR_CODES = [
  "unsupported_backend",
  "backup_option_required",
  "invalid_target",
  "legacy_schema_unsupported",
  "integrity_violation",
  "restore_required",
  "restore_required_no_backup",
  "writer_active",
  "db_newer_than_tool",
  "unsupported_resource",
  "insufficient_space",
  "backup_failed",
  "baseline_schema_mismatch",
  "migration_failed",
  "verify_failed",
  "bundle_invalid",
  "backup_incomplete",
  "backup_newer_than_tool",
  "restore_failed",
  "tool_unavailable",
  "tool_output_unrecognized",
] as const;

export type MigrationErrorCode = (typeof MIGRATION_ERROR_CODES)[number];

/** What the user can do: run the migration again, restore the reported
 * backup, start over in an empty data root, upgrade through the legacy
 * release, or nothing in-app (update Lookback / inspect logs). */
type MigrationRemedy = "retry" | "restore" | "start_fresh" | "legacy_release" | "none";

export const MIGRATION_REMEDY: Record<MigrationErrorCode, MigrationRemedy> = {
  unsupported_backend: "none",
  backup_option_required: "none",
  invalid_target: "none",
  legacy_schema_unsupported: "legacy_release",
  integrity_violation: "start_fresh",
  restore_required: "restore",
  restore_required_no_backup: "start_fresh",
  writer_active: "retry",
  db_newer_than_tool: "none",
  unsupported_resource: "none",
  insufficient_space: "retry",
  backup_failed: "retry",
  baseline_schema_mismatch: "none",
  migration_failed: "retry",
  verify_failed: "none",
  bundle_invalid: "none",
  backup_incomplete: "retry",
  backup_newer_than_tool: "none",
  restore_failed: "retry",
  tool_unavailable: "none",
  tool_output_unrecognized: "retry",
};

function isMigrationErrorCode(code: string): code is MigrationErrorCode {
  return (MIGRATION_ERROR_CODES as readonly string[]).includes(code);
}

function migrationActions(remedy: MigrationRemedy, backupPath: string): RecoveryAction[] {
  const restoreAction: DatabaseRestoreAction = {
    kind: "restore",
    id: "restore_migration",
    labelKey: "bootError.action.restoreMigration.label",
    intent: "danger",
    pendingLabelKey: "bootError.action.restoreMigration.pending",
    run: () => restoreDatabaseMigrationBackup(backupPath),
  };
  const remedyActions: Record<MigrationRemedy, RecoveryAction[]> = {
    retry: [retryMigrationAction],
    // Without a reported backup there is nothing to restore; the tool keeps
    // refusing until one is, so only the escape hatches remain.
    restore: backupPath ? [restoreAction, retryMigrationAction] : [],
    start_fresh: [startFreshSetupAction],
    legacy_release: [openMemoryKindMigrationReleaseAction],
    none: [],
  };
  return [...remedyActions[remedy], openLogAction, quitAction];
}

function DatabaseMigrationBootError({
  failure,
}: {
  failure: Extract<SidecarErrorPayload, { kind: "database_migration_failed" }>;
}) {
  const { t } = useTranslation();
  const code = failure.error_code;
  const known = isMigrationErrorCode(code);
  // An unknown code comes from a newer tool; retrying is the least harmful
  // offer because the tool refuses again if it must.
  const remedy: MigrationRemedy = known ? MIGRATION_REMEDY[code] : "retry";
  const message = t(
    known
      ? `bootError.databaseMigrationFailed.code.${code}`
      : "bootError.databaseMigrationFailed.unknownCode",
    { error_code: code },
  );
  return (
    <BootErrorShell
      title={t("bootError.databaseMigrationFailed.title")}
      body={
        <>
          <p className="boot-error-message">{message}</p>
          <pre className="boot-error-message">
            {t("bootError.databaseMigrationFailed.facts", {
              stage: failure.stage,
              error_code: failure.error_code,
              backup_path: failure.backup_path || t("bootError.databaseMigrationFailed.noBackup"),
            })}
          </pre>
          {failure.detail && <pre className="boot-error-message">{failure.detail}</pre>}
        </>
      }
      actions={migrationActions(remedy, failure.backup_path)}
    />
  );
}

/** One entry per `StartupFailureCode`. The title / body are owned here
 * (not derived from memories' `message` field) so a memories rephrase
 * never breaks the UI. Adding a new code: extend the
 * `StartupFailureCode` union in `@/types/api` and add a matching entry
 * here — the `RECOVERY_TABLE has an entry for every StartupFailureCode`
 * test guards against forgetting.
 *
 * `title` / `body` hold i18n keys (resolved with `t()` at render); `body`
 * additionally carries an interpolation-vars builder so structured fields
 * (`f.expected_dim`, …) flow into the `{{var}}` placeholders. The mapped
 * type narrows that builder's parameter to the matching variant so each
 * entry can read structured fields without re-discriminating on `f.code`. */
type RecoveryEntry<K extends StartupFailureCode> = {
  titleKey: string;
  bodyKey: string;
  bodyVars: (failure: Extract<StartupFailure, { code: K }>) => Record<string, unknown>;
  actions: RecoveryAction[];
};

type RecoveryTable = {
  [K in StartupFailureCode]: RecoveryEntry<K>;
};

export const RECOVERY_TABLE: RecoveryTable = {
  lancedb_schema_mismatch: {
    titleKey: "bootError.lancedbSchemaMismatch.title",
    bodyKey: "bootError.lancedbSchemaMismatch.body",
    bodyVars: (f) => ({ actualDim: f.actual_dim, expectedDim: f.expected_dim }),
    actions: [resetEmbeddingAction, openLogAction, quitAction],
  },
  lancedb_init_failed: {
    titleKey: "bootError.lancedbInitFailed.title",
    bodyKey: "bootError.lancedbInitFailed.body",
    bodyVars: (f) => ({ uri: f.uri, message: f.message }),
    actions: [openLogAction, quitAction],
  },
  embedding_dimension_mismatch: {
    titleKey: "bootError.embeddingDimensionMismatch.title",
    bodyKey: "bootError.embeddingDimensionMismatch.body",
    bodyVars: (f) => ({
      runnerName: f.runner_name,
      actualDim: f.actual_dim,
      expectedDim: f.expected_dim,
    }),
    actions: [resetEmbeddingAction, openLogAction, quitAction],
  },
  media_config_conflict: {
    titleKey: "bootError.mediaConfigConflict.title",
    bodyKey: "bootError.mediaConfigConflict.body",
    bodyVars: (f) => ({ backend: f.backend, imageSearchMode: f.image_search_mode }),
    actions: [openLogAction, quitAction],
  },
  rdb_pool_init_failed: {
    titleKey: "bootError.rdbPoolInitFailed.title",
    bodyKey: "bootError.rdbPoolInitFailed.body",
    bodyVars: (f) => ({ url: f.url_sanitized, message: f.message }),
    actions: [openLogAction, quitAction],
  },
  env_var_invalid: {
    titleKey: "bootError.envVarInvalid.title",
    bodyKey: "bootError.envVarInvalid.body",
    bodyVars: (f) => ({ name: f.name, message: f.message }),
    actions: [openLogAction, quitAction],
  },
  config_load_failed: {
    titleKey: "bootError.configLoadFailed.title",
    bodyKey: "bootError.configLoadFailed.body",
    bodyVars: (f) => ({ component: f.component, message: f.message }),
    actions: [openLogAction, quitAction],
  },
  other: {
    titleKey: "bootError.other.title",
    bodyKey: "bootError.other.body",
    bodyVars: (f) => ({ component: f.component, message: f.message }),
    actions: [openLogAction, quitAction],
  },
};

/**
 * Full-screen recovery surface shown when the sidecar reports a
 * startup failure. Renders code-specific guidance + actions when the
 * payload is structured; falls back to a raw-message panel otherwise.
 * The component auto-unmounts when the recovery succeeds: a successful
 * restart fires `sidecar://ready`, which flips `useSidecarStatus.phase`
 * to `"ready"`, which makes `App.tsx` swap back to the main layout.
 */
export function BootError({ failure }: { failure: SidecarErrorPayload }) {
  const { t } = useTranslation();
  if (failure.kind === "database_migration_failed") {
    return <DatabaseMigrationBootError failure={failure} />;
  }
  if (failure.kind === "raw") {
    return (
      <BootErrorShell
        title={t("bootError.raw.title")}
        // `failure.message` is the sidecar's raw error string — surfaced
        // verbatim, intentionally not translated.
        body={<pre className="boot-error-message">{failure.message}</pre>}
        actions={[openLogAction, quitAction]}
      />
    );
  }
  if (failure.kind === "recovery_failed") {
    return <StructuredBootError failure={failure.failure} recoveryError={failure.recovery_error} />;
  }
  return <StructuredBootError failure={failure.failure} />;
}

/** Per-variant lookup helper. Extracted so we can name the generic
 * parameter `K` and let TS prove that `RECOVERY_TABLE[failure.code]`'s
 * `bodyVars` accepts the matching narrowed variant. Without the explicit
 * type parameter the lookup falls back to `RecoveryEntry<StartupFailureCode>`,
 * where `bodyVars` expects `Extract<StartupFailure, {code: StartupFailureCode}>`
 * = the original union — defeating the narrowing the typed table was
 * supposed to give us. */
function StructuredBootError<K extends StartupFailureCode>({
  failure,
  recoveryError,
}: {
  failure: Extract<StartupFailure, { code: K }>;
  recoveryError?: string;
}) {
  const { t } = useTranslation();
  const entry = RECOVERY_TABLE[failure.code as K];
  const inspectEmbedding =
    failure.code === "lancedb_schema_mismatch" || failure.code === "embedding_dimension_mismatch";
  return (
    <BootErrorShell
      title={t(entry.titleKey)}
      body={
        <>
          <pre className="boot-error-message">{t(entry.bodyKey, entry.bodyVars(failure))}</pre>
          {recoveryError !== undefined && <pre className="boot-error-message">{recoveryError}</pre>}
          {inspectEmbedding && <EmbeddingMigrationPanel mode="local" compact />}
        </>
      }
      actions={entry.actions}
    />
  );
}

function BootErrorShell({
  title,
  body,
  actions,
}: {
  title: string;
  body: ReactNode;
  actions: RecoveryAction[];
}) {
  const { t } = useTranslation();
  // `pendingId` identifies which button is currently running. Used so
  // ONLY that button shows the spinner / running label; the others
  // stay labelled as before but are disabled. Sharing a single boolean
  // (the earlier implementation) made every button read the running
  // label at once, which looked like a frozen UI where every option had
  // silently become the same action.
  const [pendingId, setPendingId] = useState<ActionId | null>(null);
  const [restartError, setRestartError] = useState<string | null>(null);
  const [restoreSucceeded, setRestoreSucceeded] = useState(false);

  const onClick = async (action: RecoveryAction) => {
    setPendingId(action.id);
    setRestartError(null);
    if (action.id === "retry_migration") {
      setRestoreSucceeded(false);
    }
    try {
      if (action.kind === "restart") {
        // The restart commands return a structured `RecoveryResult`; if
        // the restart didn't settle, stay mounted on BootError and
        // surface the failure so the user can pick a different action.
        const result = await action.run();
        if (!result.restarted) {
          setRestartError(result.restartError ?? t("bootError.restartFailedFallback"));
        }
      } else if (action.kind === "restore") {
        const result = await action.run();
        if (result.state !== "safeStopped") {
          throw new Error(`unexpected database restore state: ${String(result.state)}`);
        }
        setRestoreSucceeded(true);
      } else {
        await action.run();
      }
    } catch (e) {
      setRestartError(e instanceof Error ? e.message : String(e));
    } finally {
      setPendingId(null);
    }
  };

  const pendingAction = pendingId ? (actions.find((a) => a.id === pendingId) ?? null) : null;

  return (
    <div className="boot-error">
      <div className="empty-title">{title}</div>
      <div className="empty-desc">{body}</div>
      {restartError && (
        <div className="boot-error-restart">
          {t("bootError.restartFailed", { message: restartError })}
        </div>
      )}
      {restoreSucceeded && (
        <div className="boot-error-restore-success" role="status">
          {t("bootError.databaseRestoreSucceeded")}
        </div>
      )}
      {pendingAction && pendingAction.kind !== "escape" && (
        <div className="boot-error-progress" role="status" aria-live="polite">
          <span className="saving-spinner" aria-hidden="true" />
          <span>{t(pendingAction.pendingLabelKey)}</span>
        </div>
      )}
      <div className="boot-error-actions">
        {actions.map((action) => {
          const isPending = pendingId === action.id;
          return (
            <button
              key={action.id}
              type="button"
              className={`btn ${action.intent ?? ""}`.trim()}
              // Disable every button while one is in flight to prevent
              // a second click (e.g. evacuate + purge) from racing the
              // first command's restart.
              disabled={pendingId !== null || (restoreSucceeded && action.kind === "restore")}
              onClick={() => {
                void onClick(action);
              }}
            >
              {isPending && <span className="saving-spinner" aria-hidden="true" />}
              {isPending ? t("bootError.actionRunning") : t(action.labelKey)}
            </button>
          );
        })}
      </div>
    </div>
  );
}
