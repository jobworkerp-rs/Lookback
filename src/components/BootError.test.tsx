import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { StartupFailureCode } from "@/types/api";
import { BootError, RECOVERY_TABLE } from "./BootError";

const invokeMock = vi.fn();
const openExternalMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));
vi.mock("@tauri-apps/plugin-shell", () => ({
  open: (...args: unknown[]) => openExternalMock(...args),
}));

beforeEach(() => {
  i18n.changeLanguage("ja");
  invokeMock.mockReset();
  openExternalMock.mockReset();
  openExternalMock.mockResolvedValue(undefined);
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

  it("offers retry, explicit restore, logs, and quit for migration failure", async () => {
    renderWithProviders(
      <BootError
        failure={{
          kind: "database_migration_failed",
          phase: "schema apply",
          reason: "checksum mismatch",
          backup_path: "/data/database-migration-backups/a",
        }}
      />,
    );
    expect(screen.getByText("データベースの移行に失敗しました")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "データベース移行を再試行" }));
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("retry_database_migration"));
    fireEvent.click(screen.getByRole("button", { name: "移行前のデータを復元" }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("restore_database_migration_backup", {
        backupPath: "/data/database-migration-backups/a",
      }),
    );
    expect(screen.getByRole("button", { name: "ログを開く" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "アプリを終了" })).toBeInTheDocument();
  });

  it("offers restore instead of retry while a restore is incomplete", () => {
    renderWithProviders(
      <BootError
        failure={{
          kind: "database_migration_failed",
          phase: "restore pending recovery",
          reason: "restore interrupted",
          backup_path: "/data/database-migration-backups/a",
        }}
      />,
    );

    expect(screen.getByRole("button", { name: "移行前のデータを復元" })).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "データベース移行を再試行" }),
    ).not.toBeInTheDocument();
  });

  it("keeps migration recovery safe-stopped after restore until retry is explicitly selected", async () => {
    invokeMock.mockImplementation((command: string) => {
      if (command === "restore_database_migration_backup") {
        return Promise.resolve({
          backupPath: "/data/database-migration-backups/a",
          state: "safeStopped",
        });
      }
      return Promise.resolve({ restarted: true, backupPath: null, restartError: null });
    });
    renderWithProviders(
      <BootError
        failure={{
          kind: "database_migration_failed",
          phase: "schema apply",
          reason: "checksum mismatch",
          backup_path: "/data/database-migration-backups/a",
        }}
      />,
    );

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
    expect(screen.getByRole("button", { name: "ログを開く" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "アプリを終了" })).toBeEnabled();

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
    renderWithProviders(
      <BootError
        failure={{
          kind: "database_migration_failed",
          phase: "schema apply",
          reason: "checksum mismatch",
          backup_path: "/data/database-migration-backups/a",
        }}
      />,
    );

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

  it("shows the v0.0.7 migration release action only for a migration-required database", async () => {
    renderWithProviders(
      <BootError
        failure={{
          kind: "memory_kind_migration_required",
          db_path: "/data/memories/default.sqlite3",
        }}
      />,
    );
    expect(
      screen.getByText(
        /Lookback v0\.0\.7 の移行ツールで移行してから、アプリを再起動してください。/,
      ),
    ).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "v0.0.7 の移行ツールを開く" }));
    await waitFor(() => {
      expect(openExternalMock).toHaveBeenCalledWith(
        "https://github.com/jobworkerp-rs/Lookback/releases/tag/v0.0.7",
      );
    });
  });

  it("renders a database check failure with the path and original reason", () => {
    renderWithProviders(
      <BootError
        failure={{
          kind: "memory_kind_database_check_failed",
          db_path: "/data/memories/default.sqlite3",
          reason: "unable to open database file",
        }}
      />,
    );
    expect(screen.getByText("データベースを確認できません")).toBeInTheDocument();
    expect(screen.getByText(/\/data\/memories\/default\.sqlite3/)).toBeInTheDocument();
    expect(screen.getByText(/unable to open database file/)).toBeInTheDocument();
  });

  it("invokes recover_evacuate_lancedb when the primary action is clicked", async () => {
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
    fireEvent.click(screen.getByText("ベクトル DB をバックアップして再起動"));
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("recover_evacuate_lancedb");
    });
  });

  it("surfaces restartError when the recovery action restarted=false", async () => {
    invokeMock.mockResolvedValueOnce({
      restarted: false,
      backupPath: null,
      restartError: "still failing",
    });
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
    fireEvent.click(screen.getByText("ベクトル DB をバックアップして再起動"));
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
    invokeMock.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveInvoke = resolve;
        }),
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
    const purgeLabel = "ベクトル DB を削除して再起動 (復元不可)";
    const evacuateLabel = "ベクトル DB をバックアップして再起動";
    const resetLabel = "Embedding 設定をリセットして再起動";

    fireEvent.click(screen.getByText(purgeLabel));

    // Clicked button: switches to "実行中…".
    await screen.findByRole("button", { name: /実行中/ });
    // Other buttons: keep their original label, but are disabled.
    const evacuateBtn = screen.getByRole("button", { name: evacuateLabel });
    const resetBtn = screen.getByRole("button", { name: resetLabel });
    expect(evacuateBtn).toBeDisabled();
    expect(resetBtn).toBeDisabled();
    // The action-specific progress chip is visible so the user can see
    // WHAT is taking time (sidecar restart, not a frozen UI).
    expect(screen.getByText("ベクトル DB を削除してサイドカーを再起動中…")).toBeInTheDocument();

    // Resolve the command (success-restart) and confirm the UI returns
    // to the labelled / enabled state.
    resolveInvoke?.({ restarted: true, backupPath: null, restartError: null });
    await waitFor(() => {
      expect(screen.getByRole("button", { name: purgeLabel })).toBeEnabled();
    });
    expect(
      screen.queryByText("ベクトル DB を削除してサイドカーを再起動中…"),
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
