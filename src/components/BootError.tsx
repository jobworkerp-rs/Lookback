import { open } from "@tauri-apps/plugin-dialog";
import { type ReactNode, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  migrateMemoryKind,
  openLogDir,
  openMemoryKindMigrationGuide,
  quitApp,
  recoverEvacuateLancedb,
  recoverPurgeLancedb,
  recoverResetEmbeddingSettings,
  startFreshSetup,
} from "@/api";
import type {
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
  | "evacuate"
  | "purge"
  | "reset_embedding"
  | "migrate_memory_kind"
  | "open_log"
  | "open_migration_guide"
  | "start_fresh_setup"
  | "quit";

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
  manualFallbackAction?: RecoveryEscapeAction;
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

type RecoveryAction = RecoveryRestartAction | RecoveryEscapeAction;

const evacuateAction: RecoveryRestartAction = {
  kind: "restart",
  id: "evacuate",
  labelKey: "bootError.action.evacuate.label",
  intent: "primary",
  pendingLabelKey: "bootError.action.evacuate.pending",
  run: recoverEvacuateLancedb,
};
const purgeAction: RecoveryRestartAction = {
  kind: "restart",
  id: "purge",
  labelKey: "bootError.action.purge.label",
  intent: "danger",
  pendingLabelKey: "bootError.action.purge.pending",
  run: recoverPurgeLancedb,
};
const resetEmbeddingAction: RecoveryRestartAction = {
  kind: "restart",
  id: "reset_embedding",
  labelKey: "bootError.action.resetEmbedding.label",
  pendingLabelKey: "bootError.action.resetEmbedding.pending",
  run: recoverResetEmbeddingSettings,
};
const openLogAction: RecoveryEscapeAction = {
  kind: "escape",
  id: "open_log",
  labelKey: "bootError.action.openLog.label",
  run: openLogDir,
};
const openMemoryKindMigrationGuideAction: RecoveryEscapeAction = {
  kind: "escape",
  id: "open_migration_guide",
  labelKey: "bootError.action.openMemoryKindMigrationGuide.label",
  intent: "primary",
  run: openMemoryKindMigrationGuide,
};
const migrateMemoryKindAction: RecoveryRestartAction = {
  kind: "restart",
  id: "migrate_memory_kind",
  labelKey: "bootError.action.migrateMemoryKind.label",
  intent: "primary",
  pendingLabelKey: "bootError.action.migrateMemoryKind.pending",
  run: migrateMemoryKind,
  manualFallbackAction: openMemoryKindMigrationGuideAction,
};
const startFreshSetupAction: RecoveryEscapeAction = {
  kind: "escape",
  id: "start_fresh_setup",
  labelKey: "bootError.action.startFreshSetup.label",
  intent: "primary",
  run: async () => {
    const selected = await open({ directory: true, multiple: false });
    if (typeof selected === "string") await startFreshSetup(selected);
  },
};
const quitAction: RecoveryEscapeAction = {
  kind: "escape",
  id: "quit",
  labelKey: "bootError.action.quit.label",
  run: quitApp,
};

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
    actions: [evacuateAction, purgeAction, resetEmbeddingAction],
  },
  lancedb_init_failed: {
    titleKey: "bootError.lancedbInitFailed.title",
    bodyKey: "bootError.lancedbInitFailed.body",
    bodyVars: (f) => ({ uri: f.uri, message: f.message }),
    actions: [evacuateAction, openLogAction, quitAction],
  },
  embedding_dimension_mismatch: {
    titleKey: "bootError.embeddingDimensionMismatch.title",
    bodyKey: "bootError.embeddingDimensionMismatch.body",
    bodyVars: (f) => ({
      runnerName: f.runner_name,
      actualDim: f.actual_dim,
      expectedDim: f.expected_dim,
    }),
    actions: [evacuateAction, purgeAction, resetEmbeddingAction],
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
  if (failure.kind === "memory_kind_migration_required") {
    return (
      <BootErrorShell
        title={t("bootError.memoryKindMigrationRequired.title")}
        body={
          <pre className="boot-error-message">
            {t("bootError.memoryKindMigrationRequired.body", failure)}
          </pre>
        }
        actions={[migrateMemoryKindAction, openLogAction, quitAction]}
      />
    );
  }
  if (failure.kind === "memory_kind_database_schema_invalid") {
    return (
      <BootErrorShell
        title={t("bootError.memoryKindDatabaseSchemaInvalid.title")}
        body={
          <pre className="boot-error-message">
            {t("bootError.memoryKindDatabaseSchemaInvalid.body", failure)}
          </pre>
        }
        actions={[openLogAction, quitAction]}
      />
    );
  }
  if (failure.kind === "unexpected_memory_data") {
    return (
      <BootErrorShell
        title={t("bootError.unexpectedMemoryData.title")}
        body={
          <pre className="boot-error-message">
            {t("bootError.unexpectedMemoryData.body", failure)}
          </pre>
        }
        actions={[startFreshSetupAction, openLogAction, quitAction]}
      />
    );
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
}: {
  failure: Extract<StartupFailure, { code: K }>;
}) {
  const { t } = useTranslation();
  const entry = RECOVERY_TABLE[failure.code as K];
  return (
    <BootErrorShell
      title={t(entry.titleKey)}
      body={<pre className="boot-error-message">{t(entry.bodyKey, entry.bodyVars(failure))}</pre>}
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
  const [manualFallbackFor, setManualFallbackFor] = useState<ActionId | null>(null);

  const onClick = async (action: RecoveryAction) => {
    setPendingId(action.id);
    setRestartError(null);
    setManualFallbackFor(null);
    try {
      if (action.kind === "restart") {
        // The restart commands return a structured `RecoveryResult`; if
        // the restart didn't settle, stay mounted on BootError and
        // surface the failure so the user can pick a different action.
        const result = await action.run();
        if (!result.restarted) {
          setRestartError(result.restartError ?? t("bootError.restartFailedFallback"));
        }
      } else {
        await action.run();
      }
    } catch (e) {
      setRestartError(e instanceof Error ? e.message : String(e));
      if (action.kind === "restart" && action.manualFallbackAction) {
        setManualFallbackFor(action.id);
      }
    } finally {
      setPendingId(null);
    }
  };

  const pendingAction = pendingId ? (actions.find((a) => a.id === pendingId) ?? null) : null;
  const visibleActions = actions.flatMap((action) =>
    action.kind === "restart" && manualFallbackFor === action.id && action.manualFallbackAction
      ? [action, action.manualFallbackAction]
      : [action],
  );

  return (
    <div className="boot-error">
      <div className="empty-title">{title}</div>
      <div className="empty-desc">{body}</div>
      {restartError && (
        <div className="boot-error-restart">
          {t("bootError.restartFailed", { message: restartError })}
        </div>
      )}
      {pendingAction?.kind === "restart" && (
        <div className="boot-error-progress" role="status" aria-live="polite">
          <span className="saving-spinner" aria-hidden="true" />
          <span>{t(pendingAction.pendingLabelKey)}</span>
        </div>
      )}
      <div className="boot-error-actions">
        {visibleActions.map((action) => {
          const isPending = pendingId === action.id;
          return (
            <button
              key={action.id}
              type="button"
              className={`btn ${action.intent ?? ""}`.trim()}
              // Disable every button while one is in flight to prevent
              // a second click (e.g. evacuate + purge) from racing the
              // first command's restart.
              disabled={pendingId !== null}
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
