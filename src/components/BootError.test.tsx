import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { SidecarErrorPayload, StartupFailureCode } from "@/types/api";
import { BootError, MIGRATION_ERROR_CODES, MIGRATION_REMEDY, RECOVERY_TABLE } from "./BootError";

const invokeMock = vi.fn();
const openExternalMock = vi.fn();
const openDialogMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));
vi.mock("@tauri-apps/plugin-shell", () => ({
  open: (...args: unknown[]) => openExternalMock(...args),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (...args: unknown[]) => openDialogMock(...args),
}));

function migrationFailure(
  errorCode: string,
  overrides: Partial<Extract<SidecarErrorPayload, { kind: "database_migration_failed" }>> = {},
): SidecarErrorPayload {
  return {
    kind: "database_migration_failed",
    stage: "post_migrate",
    error_code: errorCode,
    resolution: "retry",
    backup_path: "",
    detail: "",
    ...overrides,
  };
}

const restoreRequired = () =>
  migrationFailure("restore_required", {
    stage: "preflight",
    resolution: "restore_required",
    backup_path: "/data/database-migration-backups/a",
  });

beforeEach(() => {
  i18n.changeLanguage("ja");
  invokeMock.mockReset();
  openExternalMock.mockReset();
  openExternalMock.mockResolvedValue(undefined);
  openDialogMock.mockReset();
  // The lancedb / settings rewrite commands always return a
  // `RecoveryResult`; the open-log / quit ones return undefined. A
  // default-success makes the happy-path test concise.
  invokeMock.mockImplementation((command: string) =>
    Promise.resolve(
      command === "restore_database_migration_backup"
        ? { state: "safeStopped", backupPath: "/data/database-migration-backups/a" }
        : { restarted: true, backupPath: null, restartError: null },
    ),
  );
});

describe("BootError", () => {
  it("renders schema_mismatch with both dim values visible", () => {
    renderWithProviders(
      <BootError
        failure={{
          kind: "structured",
          failure: {
            code: "lancedb_schema_mismatch",
            table: "memories",
            uri: "/x",
            expected_dim: 2048,
            actual_dim: 768,
            expected_fingerprint: "",
            actual_fingerprint: "",
          },
        }}
      />,
    );
    expect(screen.getByText("Embedding ベクトルの次元が変わりました")).toBeInTheDocument();
    // Both dimensions must appear so the user can correlate the mismatch.
    expect(screen.getByText(/2048/)).toBeInTheDocument();
    expect(screen.getByText(/768/)).toBeInTheDocument();
  });

  it("renders raw fallback for kind=raw", () => {
    renderWithProviders(<BootError failure={{ kind: "raw", message: "boom" }} />);
    expect(screen.getByText("サイドカーの起動に失敗しました")).toBeInTheDocument();
    expect(screen.getByText("boom")).toBeInTheDocument();
    // Raw fallback shows only escape-hatch actions, not the lancedb ones.
    expect(screen.queryByText(/ベクトル DB をバックアップ/)).not.toBeInTheDocument();
  });

  it("renders structured recovery actions for recovery_failed payload", () => {
    renderWithProviders(
      <BootError
        failure={{
          kind: "recovery_failed",
          failure: {
            code: "lancedb_schema_mismatch",
            table: "memories",
            uri: "/x",
            expected_dim: 2048,
            actual_dim: 768,
            expected_fingerprint: "",
            actual_fingerprint: "",
          },
          recovery_error: "rollback failed verbatim",
        }}
      />,
    );

    expect(screen.getByText("Embedding ベクトルの次元が変わりました")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Embedding 設定をリセットして再起動" }),
    ).toBeInTheDocument();
    expect(screen.getByText("rollback failed verbatim")).toBeInTheDocument();
  });

  it("offers restore, retry, logs, and quit when the tool requires a restore", async () => {
    renderWithProviders(<BootError failure={restoreRequired()} />);

    expect(screen.getByText("データベースの移行に失敗しました")).toBeInTheDocument();
    expect(screen.getByText(/前回の移行が途中で失敗したため/)).toBeInTheDocument();
    expect(screen.getByText(/restore_required/)).toBeInTheDocument();
    expect(screen.getByText(/\/data\/database-migration-backups\/a/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "移行前のデータを復元" }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("restore_database_migration_backup", {
        backupPath: "/data/database-migration-backups/a",
      }),
    );
    expect(screen.getByRole("button", { name: "データベース移行を再試行" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "ログを開く" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "アプリを終了" })).toBeInTheDocument();
  });

  it("keeps migration recovery safe-stopped after restore until retry is explicitly selected", async () => {
    renderWithProviders(<BootError failure={restoreRequired()} />);

    const restore = screen.getByRole("button", { name: "移行前のデータを復元" });
    fireEvent.click(restore);

    expect(
      await screen.findByText(
        "移行前のデータを復元しました。サイドカーは停止中です。移行を再試行するか、アプリを終了してください。",
      ),
    ).toBeInTheDocument();
    expect(screen.queryByText(/サイドカーが再起動できませんでした/)).not.toBeInTheDocument();
    expect(restore).toBeDisabled();

    const retry = screen.getByRole("button", { name: "データベース移行を再試行" });
    expect(retry).toBeEnabled();
    fireEvent.click(retry);
    await waitFor(() => {
      expect(invokeMock).toHaveBeenLastCalledWith("retry_database_migration");
    });
  });

  it("clears safe-stopped status when retry starts and allows restore after retry fails", async () => {
    let resolveRetry: ((value: unknown) => void) | undefined;
    invokeMock.mockImplementation((command: string) => {
      if (command === "restore_database_migration_backup") {
        return Promise.resolve({
          backupPath: "/data/database-migration-backups/a",
          state: "safeStopped",
        });
      }
      if (command === "retry_database_migration") {
        return new Promise((resolve) => {
          resolveRetry = resolve;
        });
      }
      return Promise.resolve(undefined);
    });
    renderWithProviders(<BootError failure={restoreRequired()} />);

    const restore = screen.getByRole("button", { name: "移行前のデータを復元" });
    fireEvent.click(restore);
    await screen.findByText(/移行前のデータを復元しました/);

    fireEvent.click(screen.getByRole("button", { name: "データベース移行を再試行" }));
    await waitFor(() => {
      expect(screen.queryByText(/移行前のデータを復元しました/)).not.toBeInTheDocument();
    });
    expect(restore).toBeDisabled();

    resolveRetry?.({ restarted: false, backupPath: null, restartError: "retry failed" });
    await screen.findByText(/復旧処理に失敗しました: retry failed/);
    expect(restore).toBeEnabled();
  });

  it("offers only retry for a transient failure such as an open database", async () => {
    renderWithProviders(
      <BootError
        failure={migrationFailure("writer_active", {
          stage: "writer_check",
          detail: "another connection holds the database",
        })}
      />,
    );

    expect(screen.getByText(/他のプロセスがデータベースを開いている/)).toBeInTheDocument();
    expect(screen.getByText(/another connection holds the database/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "移行前のデータを復元" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "データベース移行を再試行" }));
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("retry_database_migration"));
  });

  it("offers a fresh data folder when a restore is required but no backup exists", async () => {
    openDialogMock.mockResolvedValue("/new/empty-root");
    renderWithProviders(
      <BootError
        failure={migrationFailure("restore_required_no_backup", {
          stage: "preflight",
          resolution: "restore_required",
        })}
      />,
    );

    expect(screen.getByText(/バックアップなしで実行された移行/)).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "データベース移行を再試行" }),
    ).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "新しい空の保存先で開始" }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("start_fresh_setup", { path: "/new/empty-root" }),
    );
    expect(openDialogMock).toHaveBeenCalledWith(expect.objectContaining({ directory: true }));
  });

  it("does nothing when the fresh data folder picker is cancelled", async () => {
    openDialogMock.mockResolvedValue(null);
    renderWithProviders(<BootError failure={migrationFailure("restore_required_no_backup")} />);

    fireEvent.click(screen.getByRole("button", { name: "新しい空の保存先で開始" }));
    await waitFor(() => expect(openDialogMock).toHaveBeenCalled());
    expect(invokeMock).not.toHaveBeenCalledWith("start_fresh_setup", expect.anything());
  });

  it("points a pre-memory_kind database at the v0.0.7 release", async () => {
    renderWithProviders(
      <BootError
        failure={migrationFailure("legacy_schema_unsupported", {
          stage: "preflight",
          resolution: "legacy_upgrade_required",
        })}
      />,
    );

    expect(screen.getByText(/Lookback v0\.0\.7 で移行してから/)).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "データベース移行を再試行" }),
    ).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "v0.0.7 の移行ツールを開く" }));
    await waitFor(() => {
      expect(openExternalMock).toHaveBeenCalledWith(
        "https://github.com/jobworkerp-rs/Lookback/releases/tag/v0.0.7",
      );
    });
  });

  it("asks for a newer app instead of retrying when the database is newer", () => {
    renderWithProviders(
      <BootError
        failure={migrationFailure("db_newer_than_tool", {
          stage: "plan",
          resolution: "tool_update_required",
        })}
      />,
    );

    expect(screen.getByText(/より新しいバージョンの Lookback/)).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "データベース移行を再試行" }),
    ).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "ログを開く" })).toBeInTheDocument();
  });

  it("falls back to a generic message and retry for an unknown error code", () => {
    renderWithProviders(
      <BootError failure={migrationFailure("brand_new_code", { stage: "post_migrate" })} />,
    );

    expect(screen.getByText(/想定外のエラーで移行できませんでした/)).toBeInTheDocument();
    expect(screen.getByText(/エラーコード: brand_new_code/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "データベース移行を再試行" })).toBeInTheDocument();
  });

  it("has a remedy and a message for every known migration error code", () => {
    for (const code of MIGRATION_ERROR_CODES) {
      expect(MIGRATION_REMEDY[code], code).toBeDefined();
      expect(
        i18n.exists(`bootError.databaseMigrationFailed.code.${code}`, { lng: "ja" }),
        code,
      ).toBe(true);
      expect(
        i18n.exists(`bootError.databaseMigrationFailed.code.${code}`, { lng: "en" }),
        code,
      ).toBe(true);
    }
  });

  it("resets only application embedding configuration when requested", async () => {
    renderWithProviders(
      <BootError
        failure={{
          kind: "structured",
          failure: {
            code: "lancedb_schema_mismatch",
            table: "memories",
            uri: "/x",
            expected_dim: 2048,
            actual_dim: 768,
            expected_fingerprint: "",
            actual_fingerprint: "",
          },
        }}
      />,
    );
    fireEvent.click(screen.getByText("Embedding 設定をリセットして再起動"));
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("recover_reset_embedding_settings");
    });
  });

  it("surfaces restartError when the recovery action restarted=false", async () => {
    invokeMock.mockImplementation((command: string) =>
      Promise.resolve(
        command === "recover_reset_embedding_settings"
          ? { restarted: false, backupPath: null, restartError: "still failing" }
          : { restarted: true, backupPath: null, restartError: null },
      ),
    );
    renderWithProviders(
      <BootError
        failure={{
          kind: "structured",
          failure: {
            code: "lancedb_schema_mismatch",
            table: "memories",
            uri: "/x",
            expected_dim: 2048,
            actual_dim: 768,
            expected_fingerprint: "",
            actual_fingerprint: "",
          },
        }}
      />,
    );
    fireEvent.click(screen.getByText("Embedding 設定をリセットして再起動"));
    await waitFor(() => {
      expect(screen.getByText(/復旧処理に失敗しました: still failing/)).toBeInTheDocument();
    });
  });

  it("invokes open_log_dir for media_config_conflict", async () => {
    renderWithProviders(
      <BootError
        failure={{
          kind: "structured",
          failure: {
            code: "media_config_conflict",
            backend: "inline",
            image_search_mode: "clip",
          },
        }}
      />,
    );
    fireEvent.click(screen.getByText("ログを開く"));
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("open_log_dir");
    });
  });

  it("shows the spinner + progress label only on the clicked button while the action is in flight", async () => {
    // Hold the command pending so we can observe the in-flight UI. The
    // earlier shared-`busy` implementation flipped every button to
    // "実行中…" simultaneously, making three different recovery paths
    // look like one identical action — exactly the freeze-like UX the
    // user reported. Pin: only the clicked button reads "実行中…", the
    // others keep their original label (but are disabled), and the
    // progress chip surfaces the per-action `pendingLabel`.
    let resolveInvoke: ((value: unknown) => void) | undefined;
    invokeMock.mockImplementation((command: string) =>
      command === "recover_reset_embedding_settings"
        ? new Promise((resolve) => {
            resolveInvoke = resolve;
          })
        : Promise.resolve({ restarted: true, backupPath: null, restartError: null }),
    );

    renderWithProviders(
      <BootError
        failure={{
          kind: "structured",
          failure: {
            code: "lancedb_schema_mismatch",
            table: "memories",
            uri: "/x",
            expected_dim: 2048,
            actual_dim: 768,
            expected_fingerprint: "",
            actual_fingerprint: "",
          },
        }}
      />,
    );
    const resetLabel = "Embedding 設定をリセットして再起動";
    fireEvent.click(screen.getByText(resetLabel));
    await screen.findByRole("button", { name: /実行中/ });
    expect(screen.getByRole("button", { name: "ログを開く" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "アプリを終了" })).toBeDisabled();
    expect(
      screen.getByText("Embedding 設定をリセットしてサイドカーを再起動中…"),
    ).toBeInTheDocument();
    resolveInvoke?.({ restarted: true, backupPath: null, restartError: null });
    await waitFor(() => {
      expect(screen.getByRole("button", { name: resetLabel })).toBeEnabled();
    });
    expect(
      screen.queryByText("Embedding 設定をリセットしてサイドカーを再起動中…"),
    ).not.toBeInTheDocument();
  });

  /**
   * **Degradation regression**. Every `StartupFailureCode` must have a
   * matching entry in `RECOVERY_TABLE`, otherwise adding a new code in
   * `@/types/api` would silently land in production with the BootError
   * rendering `undefined` at runtime.
   */
  it("RECOVERY_TABLE has an entry for every StartupFailureCode", () => {
    const codes: StartupFailureCode[] = [
      "lancedb_schema_mismatch",
      "lancedb_init_failed",
      "embedding_dimension_mismatch",
      "media_config_conflict",
      "rdb_pool_init_failed",
      "env_var_invalid",
      "config_load_failed",
      "other",
    ];
    for (const c of codes) {
      const entry = RECOVERY_TABLE[c];
      expect(entry, `missing entry for ${c}`).toBeDefined();
      // The table now holds i18n keys; assert the key exists and resolves to
      // a non-empty Japanese title under the ja dictionary.
      expect(entry.titleKey.length).toBeGreaterThan(0);
      expect(i18n.t(entry.titleKey).length).toBeGreaterThan(0);
      expect(entry.actions.length).toBeGreaterThan(0);
    }
  });
});

describe("service-owned vector storage", () => {
  it("offers no filesystem mutation actions for a vector-store failure", () => {
    renderWithProviders(
      <BootError
        failure={{
          kind: "structured",
          failure: {
            code: "lancedb_schema_mismatch",
            table: "memories",
            uri: "/server-owned",
            expected_dim: 2048,
            actual_dim: 768,
            expected_fingerprint: "",
            actual_fingerprint: "",
          },
        }}
      />,
    );
    expect(
      screen.queryByRole("button", { name: /ベクトル DB をバックアップ/ }),
    ).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /ベクトル DB を削除/ })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /ログ/ })).toBeInTheDocument();
  });
});
