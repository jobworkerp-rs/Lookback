import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { StartupFailureCode } from "@/types/api";
import { BootError, RECOVERY_TABLE } from "./BootError";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

beforeEach(() => {
  i18n.changeLanguage("ja");
  invokeMock.mockReset();
  // The lancedb / settings rewrite commands always return a
  // `RecoveryResult`; the open-log / quit ones return undefined. A
  // default-success makes the happy-path test concise.
  invokeMock.mockResolvedValue({ restarted: true, backupPath: null, restartError: null });
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

  it("previews an unmigrated database before the destructive migration is approved", async () => {
    invokeMock.mockResolvedValueOnce({
      warningCount: 2,
      totalRecordCount: 5,
      unresolvedMemoryCount: 1,
      unresolvedThreadCount: 1,
      plannedMemoryDeleteCount: 1,
      plannedThreadDeleteCount: 1,
      plannedMemoryIds: [10],
      plannedThreadIds: [7],
      relatedDeletionCounts: { thread_memory: 2 },
      requiresConfirmation: true,
    });
    renderWithProviders(
      <BootError
        failure={{
          kind: "memory_kind_migration_required",
          db_path: "/data/memories/default.sqlite3",
        }}
      />,
    );
    expect(screen.getByText("メモリデータの移行が必要です")).toBeInTheDocument();
    expect(screen.getByText(/SQLite のバックアップ/)).toBeInTheDocument();
    fireEvent.click(screen.getByText("移行を実行"));
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("preview_memory_kind_migration");
    });
    expect(screen.getByText("移行内容を確認")).toBeInTheDocument();
    expect(screen.getByText(/未解決 warning: 2 件/)).toBeInTheDocument();
    expect(screen.getByText("関連行: thread_memory — 2 件")).toBeInTheDocument();
    fireEvent.click(screen.getByText("ダンプして削除・移行を実行"));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("migrate_memory_kind", {
        approval: expect.objectContaining({ plannedMemoryIds: [10], plannedThreadIds: [7] }),
      }),
    );
    expect(screen.queryByText("手動移行手順を開く")).not.toBeInTheDocument();
  });

  it("cancels the destructive confirmation without calling the migration command", async () => {
    invokeMock.mockResolvedValueOnce({
      warningCount: 1,
      totalRecordCount: 1,
      unresolvedMemoryCount: 1,
      unresolvedThreadCount: 0,
      plannedMemoryDeleteCount: 1,
      plannedThreadDeleteCount: 0,
      plannedMemoryIds: [1],
      plannedThreadIds: [],
      relatedDeletionCounts: {},
      requiresConfirmation: true,
    });
    renderWithProviders(
      <BootError failure={{ kind: "memory_kind_migration_required", db_path: "/db" }} />,
    );
    fireEvent.click(screen.getByText("移行を実行"));
    await screen.findByText("移行内容を確認");
    fireEvent.click(screen.getByText("キャンセル"));
    expect(invokeMock).not.toHaveBeenCalledWith("migrate_memory_kind");
  });

  it("offers the manual migration guide when preview fails", async () => {
    invokeMock.mockRejectedValueOnce(new Error("bundled migration failed"));
    renderWithProviders(
      <BootError
        failure={{
          kind: "memory_kind_migration_required",
          db_path: "/data/memories/default.sqlite3",
        }}
      />,
    );

    expect(screen.queryByText("手動移行手順を開く")).not.toBeInTheDocument();
    fireEvent.click(screen.getByText("移行を実行"));
    await waitFor(() => {
      expect(screen.getByText(/bundled migration failed/)).toBeInTheDocument();
    });
    fireEvent.click(screen.getByText("手動移行手順を開く"));
    expect(invokeMock).toHaveBeenCalledWith("open_memory_kind_migration_guide");
    expect(invokeMock).not.toHaveBeenCalledWith("migrate_memory_kind");
  });

  it("refuses automatic migration for unexpected owner evidence", () => {
    renderWithProviders(
      <BootError
        failure={{
          kind: "unexpected_memory_data",
          db_path: "/data/memories/default.sqlite3",
          reason: "memory.user_id is not a Lookback owner",
        }}
      />,
    );
    expect(screen.getByText("想定外のメモリデータが見つかりました")).toBeInTheDocument();
    expect(screen.queryByText("移行を実行")).not.toBeInTheDocument();
    expect(screen.getByText("新しい空の保存先で開始")).toBeInTheDocument();
    expect(screen.queryByText("手動移行手順を開く")).not.toBeInTheDocument();
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
