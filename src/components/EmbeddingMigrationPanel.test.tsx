import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import type {
  EmbeddingMigrationFailure,
  EmbeddingMigrationInspect,
  EmbeddingVerificationCounts,
} from "@/types/api";
import {
  EmbeddingMigrationPanel,
  embeddingMigrationInspectRefetchMs,
} from "./EmbeddingMigrationPanel";

const inspectEmbeddingMigration = vi.fn();
const recoverEmbeddingMigration = vi.fn();
const startEmbeddingRebuild = vi.fn();
const startEmbeddingReconcile = vi.fn();
const getEmbeddingSpace = vi.fn();
const getEmbeddingRebuildProgress = vi.fn();

vi.mock("@/api", () => ({
  inspectEmbeddingMigration: (...args: unknown[]) => inspectEmbeddingMigration(...args),
  recoverEmbeddingMigration: (...args: unknown[]) => recoverEmbeddingMigration(...args),
  startEmbeddingRebuild: (...args: unknown[]) => startEmbeddingRebuild(...args),
  startEmbeddingReconcile: (...args: unknown[]) => startEmbeddingReconcile(...args),
  getEmbeddingSpace: (...args: unknown[]) => getEmbeddingSpace(...args),
  getEmbeddingRebuildProgress: (...args: unknown[]) => getEmbeddingRebuildProgress(...args),
}));

function renderPanel(mode: "local" | "remote" = "local") {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <EmbeddingMigrationPanel mode={mode} />
      </QueryClientProvider>
    </I18nextProvider>,
  );
  return client;
}

function inspectResult(overrides: Partial<EmbeddingMigrationInspect> = {}): {
  kind: "inspect";
  data: EmbeddingMigrationInspect;
} {
  return {
    kind: "inspect",
    data: {
      state: "migrating",
      space: { kind: "id", value: "space-a" },
      required: 10,
      complete: 3,
      missing: 7,
      stale: 0,
      unverified: 0,
      failed_transient: 0,
      failed_permanent: 0,
      orphan: 0,
      backup_supported: true,
      attempt: { attempt: "attempt-1", stage: "rebuild_pending", backup_mode: "none" },
      backup: null,
      unavailable_reason: null,
      next_action: "start_rebuild",
      ...overrides,
    },
  };
}

function verificationCounts(
  overrides: Partial<EmbeddingVerificationCounts> = {},
): EmbeddingVerificationCounts {
  return {
    missing_memory_text: 0,
    stale_memory_text: 0,
    unverified_memory_text: 0,
    failed_memory_text: 0,
    missing_memory_media: 0,
    stale_memory_media: 0,
    unverified_memory_media: 0,
    failed_memory_media: 0,
    missing_thread: 0,
    stale_thread: 0,
    unverified_thread: 0,
    failed_thread: 0,
    missing_reflection_intent: 0,
    stale_reflection_intent: 0,
    unverified_reflection_intent: 0,
    failed_reflection_intent: 0,
    orphan: 0,
    ...overrides,
  };
}

function finalizeFailure(
  errorCode: string,
  resolution: EmbeddingMigrationFailure["resolution"],
  attemptId = "attempt-1",
  counts = verificationCounts(),
): { kind: "failed"; data: EmbeddingMigrationFailure } {
  return {
    kind: "failed",
    data: {
      command: "embedding_finalize",
      stage: "verify",
      error_code: errorCode,
      resolution,
      attempt: { attempt: attemptId, stage: "rebuild_pending", backup_mode: "backup" },
      backup: "/backup",
      cancel_operation: null,
      verification_counts: counts,
    },
  };
}

describe("EmbeddingMigrationPanel", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    inspectEmbeddingMigration.mockReset();
    recoverEmbeddingMigration.mockReset().mockResolvedValue({
      result: { kind: "restored", data: { attempt: "attempt-1", next_action: "reconcile" } },
      restarted: true,
    });
    startEmbeddingRebuild
      .mockReset()
      .mockResolvedValue({ attempt_id: "attempt-1", task_id: "task-1" });
    startEmbeddingReconcile.mockReset().mockResolvedValue({ task_id: "task-2" });
    getEmbeddingRebuildProgress.mockReset().mockResolvedValue({
      attempt_id: "attempt-1",
      status: "running",
      kinds: [],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: false,
      failed: 0,
      write_rejections: {},
      error: null,
    });
    getEmbeddingSpace.mockReset().mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [],
      rpc_migration_supported: true,
      write_rejections: {},
    });
  });

  it("lists discarded embedding results without warning styling", async () => {
    inspectEmbeddingMigration.mockResolvedValue(
      inspectResult({ state: "consistent", attempt: null, next_action: undefined }),
    );
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [],
      rpc_migration_supported: true,
      write_rejections: { source_version_mismatch: 1, space_mismatch: 2, attempt_mismatch: 0 },
    });
    renderPanel();

    expect(await screen.findByText("Embedding 書込みの記録")).toBeInTheDocument();
    const advisory = screen.getByText("元データの更新後に届いた古い結果: 1 件");
    expect(advisory).not.toHaveStyle({ color: "var(--warning)" });
    const warning = screen.getByText(
      "別の Embedding 空間（モデル・次元）の結果: 2 件（設定を確認してください）",
    );
    expect(warning).toHaveStyle({ color: "var(--warning)" });
    expect(screen.queryByText(/以前の再生成試行の結果/)).not.toBeInTheDocument();
  });

  it("omits the write record section when no write was rejected", async () => {
    inspectEmbeddingMigration.mockResolvedValue(
      inspectResult({ state: "consistent", attempt: null, next_action: undefined }),
    );
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [],
      rpc_migration_supported: true,
      write_rejections: { source_version_mismatch: 0 },
    });
    const client = renderPanel();

    await waitFor(() =>
      expect(client.getQueryData(["embedding-space", "migration-panel", "local"])).toBeDefined(),
    );
    expect(screen.queryByText("Embedding 書込みの記録")).not.toBeInTheDocument();
  });

  it("shows the remote server's write record read-only without local recovery inspection", async () => {
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [],
      rpc_migration_supported: false,
      write_rejections: { source_version_mismatch: 1 },
    });
    renderPanel("remote");

    expect(await screen.findByText("元データの更新後に届いた古い結果: 1 件")).toBeInTheDocument();
    expect(screen.getByText("Embedding 書込みの記録")).toBeInTheDocument();
    expect(screen.getByText(/Remote server の状態は読み取り専用です/)).toBeInTheDocument();
    expect(inspectEmbeddingMigration).not.toHaveBeenCalled();
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
  });

  it("omits the whole card for a remote connection with nothing to report", async () => {
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [],
      rpc_migration_supported: false,
      write_rejections: { source_version_mismatch: 0 },
    });
    const client = renderPanel("remote");

    await waitFor(() =>
      expect(client.getQueryData(["embedding-space", "migration-panel", "remote"])).toBeDefined(),
    );
    expect(screen.queryByRole("region", { name: "Embedding 移行の状態" })).not.toBeInTheDocument();
    expect(screen.queryByText("Embedding 移行の状態")).not.toBeInTheDocument();
  });

  it("offers start or continue for a formal migrating rebuild-pending inspection", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "inspect",
      data: {
        state: "migrating",
        space: { kind: "id", value: "space-a" },
        required: 10,
        complete: 0,
        missing: 10,
        stale: 0,
        unverified: 0,
        failed_transient: 0,
        failed_permanent: 0,
        orphan: 0,
        backup_supported: true,
        attempt: { attempt: "attempt-1", stage: "rebuild_pending", backup_mode: "backup" },
        backup: "/backup",
        unavailable_reason: null,
        next_action: "start_rebuild",
      },
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "Embedding 再生成を開始／続行" }));

    await waitFor(() =>
      expect(startEmbeddingRebuild).toHaveBeenCalledWith({ attempt_id: "attempt-1" }),
    );
    expect(recoverEmbeddingMigration).not.toHaveBeenCalled();
  });

  it("offers confirmed restore only for an inspected backed-up replace attempt", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "inspect",
      data: {
        state: "migrating",
        space: { kind: "id", value: "space-a" },
        required: 10,
        complete: 3,
        missing: 7,
        stale: 0,
        unverified: 0,
        failed_transient: 0,
        failed_permanent: 0,
        orphan: 0,
        backup_supported: true,
        attempt: { attempt: "attempt-1", stage: "replace", backup_mode: "backup" },
        backup: "/backup/attempt-1",
        unavailable_reason: null,
        next_action: "resume_or_restore",
      },
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "バックアップから復元" }));
    expect(await screen.findByRole("dialog")).toBeInTheDocument();
    expect(recoverEmbeddingMigration).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "復元する" }));

    await waitFor(() =>
      expect(recoverEmbeddingMigration).toHaveBeenCalledWith({
        action: "restore",
        attempt_id: "attempt-1",
      }),
    );
  });

  it.each([
    {
      name: "backup-stage backup mode allows resume and abandon, not restore",
      inspect: {
        state: "migrating",
        attempt: { attempt: "attempt-1", stage: "backup", backup_mode: "backup" },
        backup: null,
        next_action: "resume_or_abandon",
      },
      visible: ["移行を再開", "移行を中止"],
      hidden: ["バックアップから復元", "Embedding 再生成を開始／続行", "移行を確定"],
    },
    {
      name: "backup-stage no-backup mode allows resume and abandon",
      inspect: {
        state: "migrating",
        attempt: { attempt: "attempt-1", stage: "backup", backup_mode: "none" },
        backup: null,
        next_action: "resume_or_abandon",
      },
      visible: ["移行を再開", "移行を中止"],
      hidden: ["バックアップから復元", "Embedding 再生成を開始／続行", "移行を確定"],
    },
    {
      name: "replace with backup allows resume and restore only",
      inspect: {
        state: "migrating",
        attempt: { attempt: "attempt-1", stage: "replace", backup_mode: "backup" },
        backup: "/backup/attempt-1",
        next_action: "resume_or_restore",
      },
      visible: ["移行を再開", "バックアップから復元"],
      hidden: ["移行を中止", "Embedding 再生成を開始／続行", "移行を確定"],
    },
    {
      name: "replace without backup allows resume and abandon only",
      inspect: {
        state: "migrating",
        attempt: { attempt: "attempt-1", stage: "replace", backup_mode: "none" },
        backup: null,
        next_action: "resume_or_abandon",
      },
      visible: ["移行を再開", "移行を中止"],
      hidden: ["バックアップから復元", "Embedding 再生成を開始／続行", "移行を確定"],
    },
    {
      name: "rebuild-pending with backup allows rebuild and restore",
      inspect: {
        state: "migrating",
        attempt: { attempt: "attempt-1", stage: "rebuild_pending", backup_mode: "backup" },
        backup: "/backup/attempt-1",
        next_action: "start_rebuild",
      },
      visible: ["バックアップから復元", "Embedding 再生成を開始／続行"],
      hidden: ["移行を再開", "移行を中止", "移行を確定"],
    },
    {
      name: "rebuild-pending without backup allows rebuild and abandon",
      inspect: {
        state: "migrating",
        attempt: { attempt: "attempt-1", stage: "rebuild_pending", backup_mode: "none" },
        backup: null,
        next_action: "start_rebuild",
      },
      visible: ["移行を中止", "Embedding 再生成を開始／続行"],
      hidden: ["移行を再開", "バックアップから復元", "移行を確定"],
    },
    {
      name: "commit with backup allows finalize and restore without a progress RPC",
      inspect: {
        state: "migrating",
        attempt: { attempt: "attempt-1", stage: "commit", backup_mode: "backup" },
        backup: "/backup/attempt-1",
        next_action: "finalize",
      },
      visible: ["移行を確定", "バックアップから復元"],
      hidden: ["移行を再開", "移行を中止", "Embedding 再生成を開始／続行"],
    },
    {
      name: "commit without backup allows finalize only",
      inspect: {
        state: "migrating",
        attempt: { attempt: "attempt-1", stage: "commit", backup_mode: "none" },
        backup: null,
        next_action: "finalize",
      },
      visible: ["移行を確定"],
      hidden: ["移行を再開", "バックアップから復元", "移行を中止", "Embedding 再生成を開始／続行"],
    },
    {
      name: "restoring permits only restore",
      inspect: {
        state: "migrating",
        attempt: { attempt: "attempt-1", stage: "restoring", backup_mode: "backup" },
        backup: "/backup/attempt-1",
        next_action: "restore",
      },
      visible: ["バックアップから復元"],
      hidden: ["移行を再開", "移行を中止", "Embedding 再生成を開始／続行", "移行を確定"],
    },
    {
      name: "discarding permits only abandon",
      inspect: {
        state: "migrating",
        attempt: { attempt: "attempt-1", stage: "discarding", backup_mode: "none" },
        backup: null,
        next_action: "abandon",
      },
      visible: ["移行を中止"],
      hidden: ["移行を再開", "バックアップから復元", "Embedding 再生成を開始／続行", "移行を確定"],
    },
  ] satisfies {
    name: string;
    inspect: Partial<EmbeddingMigrationInspect>;
    visible: string[];
    hidden: string[];
  }[])(
    "follows the stage/backup-mode authority table: $name",
    async ({ inspect, visible, hidden }) => {
      inspectEmbeddingMigration.mockResolvedValue(inspectResult(inspect));
      renderPanel();

      await screen.findByText(/状態: migrating/);
      for (const name of visible) {
        expect(await screen.findByRole("button", { name })).toBeEnabled();
      }
      for (const name of hidden) {
        expect(screen.queryByRole("button", { name })).not.toBeInTheDocument();
      }
    },
  );

  it.each([
    {
      name: "a non-migrating state cannot authorize rebuild",
      inspect: { state: "consistent", next_action: "start_rebuild" },
    },
    {
      name: "replace backup mode cannot claim the no-backup action",
      inspect: {
        state: "migrating",
        attempt: { attempt: "attempt-1", stage: "replace", backup_mode: "backup" },
        backup: "/backup/attempt-1",
        next_action: "resume_or_abandon",
      },
    },
    {
      name: "rebuild-pending no-backup attempt cannot authorize restore",
      inspect: {
        state: "migrating",
        attempt: { attempt: "attempt-1", stage: "rebuild_pending", backup_mode: "none" },
        backup: null,
        next_action: "restore",
      },
    },
    {
      name: "a migrating action without its inspected attempt fails closed",
      inspect: { state: "migrating", attempt: null, next_action: "start_rebuild" },
    },
  ] satisfies { name: string; inspect: Partial<EmbeddingMigrationInspect> }[])(
    "fails closed for inconsistent inspection: $name",
    async ({ inspect }) => {
      inspectEmbeddingMigration.mockResolvedValue(inspectResult(inspect));
      renderPanel();

      expect(await screen.findByRole("alert")).toHaveTextContent(/状態応答が不正です/);
      expect(screen.queryByRole("button", { name: "移行を再開" })).not.toBeInTheDocument();
      expect(
        screen.queryByRole("button", { name: "バックアップから復元" }),
      ).not.toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "移行を中止" })).not.toBeInTheDocument();
      expect(
        screen.queryByRole("button", { name: "Embedding 再生成を開始／続行" }),
      ).not.toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "移行を確定" })).not.toBeInTheDocument();
    },
  );

  it("offers rebuild continuation and abandon for an inspected no-backup rebuild-pending attempt", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "inspect",
      data: {
        state: "migrating",
        space: { kind: "id", value: "space-a" },
        required: 10,
        complete: 3,
        missing: 7,
        stale: 0,
        unverified: 0,
        failed_transient: 0,
        failed_permanent: 0,
        orphan: 0,
        backup_supported: false,
        attempt: { attempt: "attempt-1", stage: "rebuild_pending", backup_mode: "none" },
        backup: null,
        unavailable_reason: null,
        next_action: "start_rebuild",
      },
    });
    renderPanel();

    expect(
      await screen.findByRole("button", { name: "Embedding 再生成を開始／続行" }),
    ).toBeEnabled();
    expect(await screen.findByRole("button", { name: "移行を中止" })).toBeEnabled();
    expect(screen.queryByRole("button", { name: "バックアップから復元" })).not.toBeInTheDocument();
  });

  it("confirms abandon and accepts only the abandoned result for a no-backup attempt", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "inspect",
      data: {
        state: "migrating",
        space: { kind: "id", value: "space-a" },
        required: 10,
        complete: 3,
        missing: 7,
        stale: 0,
        unverified: 0,
        failed_transient: 0,
        failed_permanent: 0,
        orphan: 0,
        backup_supported: false,
        attempt: { attempt: "attempt-1", stage: "rebuild_pending", backup_mode: "none" },
        backup: null,
        unavailable_reason: null,
        next_action: "start_rebuild",
      },
    });
    recoverEmbeddingMigration.mockResolvedValue({
      result: {
        kind: "abandoned",
        data: { outcome: "abandoned", attempt: "attempt-1", next_action: "none" },
      },
      restarted: true,
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "移行を中止" }));
    fireEvent.click(screen.getByRole("button", { name: "中止する" }));

    await waitFor(() =>
      expect(recoverEmbeddingMigration).toHaveBeenCalledWith({
        action: "abandon",
        attempt_id: "attempt-1",
      }),
    );
  });

  it("allows confirmed abandon during backup before vectors have been replaced", async () => {
    inspectEmbeddingMigration.mockResolvedValue(
      inspectResult({
        attempt: { attempt: "attempt-1", stage: "backup", backup_mode: "backup" },
        backup: null,
        next_action: "resume_or_abandon",
      }),
    );
    recoverEmbeddingMigration.mockResolvedValue({
      result: {
        kind: "abandoned",
        data: { outcome: "abandoned", attempt: "attempt-1", next_action: "none" },
      },
      restarted: true,
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "移行を中止" }));
    fireEvent.click(await screen.findByRole("button", { name: "中止する" }));

    await waitFor(() =>
      expect(recoverEmbeddingMigration).toHaveBeenCalledWith({
        action: "abandon",
        attempt_id: "attempt-1",
      }),
    );
  });

  it("requires a switched result for resume before showing success", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "inspect",
      data: {
        state: "migrating",
        space: { kind: "id", value: "space-a" },
        required: 10,
        complete: 3,
        missing: 7,
        stale: 0,
        unverified: 0,
        failed_transient: 0,
        failed_permanent: 0,
        orphan: 0,
        backup_supported: false,
        attempt: { attempt: "attempt-1", stage: "replace", backup_mode: "none" },
        backup: null,
        unavailable_reason: null,
        next_action: "resume_or_abandon",
      },
    });
    recoverEmbeddingMigration.mockResolvedValue({
      result: {
        kind: "switched",
        data: {
          outcome: "switched",
          attempt: "attempt-1",
          source_space: { kind: "id", value: "source" },
          target_space: "space-a",
          backup: null,
          next_action: "start_rebuild",
        },
      },
      restarted: true,
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "移行を再開" }));

    await waitFor(() =>
      expect(recoverEmbeddingMigration).toHaveBeenCalledWith({
        action: "resume",
        attempt_id: "attempt-1",
      }),
    );
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("shows failed counts and retries a migrating rebuild-pending attempt", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "inspect",
      data: {
        state: "migrating",
        space: { kind: "id", value: "space-a" },
        required: 12,
        complete: 8,
        missing: 0,
        stale: 0,
        unverified: 0,
        failed_transient: 2,
        failed_permanent: 2,
        orphan: 0,
        backup_supported: true,
        attempt: { attempt: "attempt-1", stage: "rebuild_pending", backup_mode: "none" },
        backup: null,
        unavailable_reason: null,
        next_action: "start_rebuild",
      },
    });
    renderPanel();

    expect(await screen.findByText(/一時失敗 2/)).toBeInTheDocument();
    expect(screen.getByText(/恒久失敗 2/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /失敗分を再試行/ }));

    await waitFor(() =>
      expect(startEmbeddingRebuild).toHaveBeenCalledWith({
        attempt_id: "attempt-1",
        retry_failed: true,
      }),
    );
  });

  it("does not offer the first finalize until matching rebuild progress expects completion", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "inspect",
      data: {
        state: "migrating",
        space: { kind: "id", value: "space-a" },
        required: 10,
        complete: 10,
        missing: 0,
        stale: 0,
        unverified: 0,
        failed_transient: 0,
        failed_permanent: 0,
        orphan: 0,
        backup_supported: true,
        attempt: { attempt: "attempt-1", stage: "rebuild_pending", backup_mode: "backup" },
        backup: "/backup",
        unavailable_reason: null,
        next_action: "start_rebuild",
      },
    });
    getEmbeddingRebuildProgress.mockResolvedValue({
      attempt_id: "attempt-1",
      status: "running",
      kinds: [],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: false,
      failed: 0,
      write_rejections: {},
      error: null,
    });
    renderPanel();

    await screen.findByText(/migrating/i);
    expect(screen.queryByRole("button", { name: "移行を確定" })).not.toBeInTheDocument();
    expect(recoverEmbeddingMigration).not.toHaveBeenCalled();
  });

  it("does not authorize the first finalize from progress belonging to a different attempt", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "inspect",
      data: {
        state: "migrating",
        space: { kind: "id", value: "space-a" },
        required: 10,
        complete: 10,
        missing: 0,
        stale: 0,
        unverified: 0,
        failed_transient: 0,
        failed_permanent: 0,
        orphan: 0,
        backup_supported: true,
        attempt: { attempt: "attempt-current", stage: "rebuild_pending", backup_mode: "backup" },
        backup: "/backup/current",
        unavailable_reason: null,
        next_action: "start_rebuild",
      },
    });
    getEmbeddingRebuildProgress.mockResolvedValue({
      attempt_id: "attempt-stale",
      status: "complete",
      kinds: [],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: true,
      failed: 0,
      write_rejections: {},
      error: null,
    });
    renderPanel();

    expect(screen.queryByRole("button", { name: "移行を確定" })).not.toBeInTheDocument();
    expect(recoverEmbeddingMigration).not.toHaveBeenCalled();
  });

  it("allows first rebuild-pending finalize when fresh state is confirmed and same-attempt progress is complete, even if counts are unknown", async () => {
    inspectEmbeddingMigration.mockResolvedValue(
      inspectResult({
        required: null,
        complete: null,
        missing: null,
        stale: null,
        unverified: null,
        failed_transient: null,
        failed_permanent: null,
        orphan: null,
        attempt: { attempt: "attempt-counts", stage: "rebuild_pending", backup_mode: "none" },
      }),
    );
    getEmbeddingRebuildProgress.mockResolvedValue({
      attempt_id: "attempt-counts",
      status: "complete",
      kinds: [],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: true,
      failed: 0,
      write_rejections: {},
      error: null,
    });
    renderPanel();

    await screen.findByText(/状態: migrating/);
    expect(await screen.findByRole("button", { name: "移行を確定" })).toBeEnabled();
    expect(recoverEmbeddingMigration).not.toHaveBeenCalled();
  });

  it("successfully finalizes an unknown-count rebuild-pending attempt without inventing an accepted failure count", async () => {
    inspectEmbeddingMigration.mockResolvedValue(
      inspectResult({
        required: null,
        complete: null,
        missing: null,
        stale: null,
        unverified: null,
        failed_transient: null,
        failed_permanent: null,
        orphan: null,
        attempt: {
          attempt: "attempt-unknown-finalize",
          stage: "rebuild_pending",
          backup_mode: "none",
        },
      }),
    );
    getEmbeddingRebuildProgress.mockResolvedValue({
      attempt_id: "attempt-unknown-finalize",
      status: "complete",
      kinds: [],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: true,
      failed: 0,
      write_rejections: {},
      error: null,
    });
    recoverEmbeddingMigration.mockResolvedValue({
      result: {
        kind: "finalized",
        data: {
          outcome: "finalized",
          attempt: "attempt-unknown-finalize",
          space: "space-a",
          accepted_failed: 0,
          next_action: "apply",
        },
      },
      restarted: true,
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "移行を確定" }));

    await waitFor(() =>
      expect(recoverEmbeddingMigration).toHaveBeenCalledWith({
        action: "finalize",
        attempt_id: "attempt-unknown-finalize",
      }),
    );
  });

  it("finalizes a commit-stage attempt without requiring counts or rebuild progress", async () => {
    inspectEmbeddingMigration.mockResolvedValue(
      inspectResult({
        required: null,
        complete: null,
        missing: null,
        stale: null,
        unverified: null,
        failed_transient: null,
        failed_permanent: null,
        orphan: null,
        attempt: { attempt: "attempt-commit", stage: "commit", backup_mode: "none" },
        next_action: "finalize",
      }),
    );
    recoverEmbeddingMigration.mockResolvedValue({
      result: {
        kind: "finalized",
        data: {
          outcome: "finalized",
          attempt: "attempt-commit",
          space: "space-a",
          accepted_failed: 0,
          next_action: "apply",
        },
      },
      restarted: true,
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "移行を確定" }));
    await waitFor(() =>
      expect(recoverEmbeddingMigration).toHaveBeenCalledWith({
        action: "finalize",
        attempt_id: "attempt-commit",
      }),
    );
    expect(getEmbeddingRebuildProgress).not.toHaveBeenCalled();
  });

  it("rechecks same-attempt completion progress before the first finalize", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "inspect",
      data: {
        state: "migrating",
        space: { kind: "id", value: "space-a" },
        required: 10,
        complete: 10,
        missing: 0,
        stale: 0,
        unverified: 0,
        failed_transient: 0,
        failed_permanent: 0,
        orphan: 0,
        backup_supported: true,
        attempt: { attempt: "attempt-finalize", stage: "rebuild_pending", backup_mode: "backup" },
        backup: "/backup/attempt-finalize",
        unavailable_reason: null,
        next_action: "start_rebuild",
      },
    });
    const progress = (attemptId: string) => ({
      attempt_id: attemptId,
      status: "complete",
      kinds: [],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: true,
      failed: 0,
      write_rejections: {},
      error: null,
    });
    getEmbeddingRebuildProgress.mockResolvedValueOnce(progress("attempt-finalize"));
    getEmbeddingRebuildProgress.mockResolvedValueOnce(progress("attempt-stale"));
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "移行を確定" }));

    expect(await screen.findByText(/現在の再生成進捗では/)).toBeInTheDocument();
    expect(recoverEmbeddingMigration).not.toHaveBeenCalled();
  });

  it("rechecks authorization before restoring the inspected attempt", async () => {
    inspectEmbeddingMigration
      .mockResolvedValueOnce({
        kind: "inspect",
        data: {
          state: "migrating",
          space: { kind: "id", value: "space-a" },
          required: 10,
          complete: 3,
          missing: 7,
          stale: 0,
          unverified: 0,
          failed_transient: 0,
          failed_permanent: 0,
          orphan: 0,
          backup_supported: true,
          attempt: { attempt: "attempt-1", stage: "replace", backup_mode: "backup" },
          backup: "/backup",
          unavailable_reason: null,
          next_action: "resume_or_restore",
        },
      })
      .mockResolvedValueOnce({
        kind: "inspect",
        data: {
          state: "consistent",
          space: { kind: "id", value: "space-a" },
          required: 10,
          complete: 10,
          missing: 0,
          stale: 0,
          unverified: 0,
          failed_transient: 0,
          failed_permanent: 0,
          orphan: 0,
          backup_supported: true,
          attempt: null,
          backup: null,
          unavailable_reason: null,
          next_action: "none",
        },
      });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "バックアップから復元" }));
    fireEvent.click(await screen.findByRole("button", { name: "復元する" }));

    expect(await screen.findByText(/移行状態またはattemptが変更されました/)).toBeInTheDocument();
    expect(recoverEmbeddingMigration).not.toHaveBeenCalled();
  });

  it("retries an ordinary failed inspection without a migration attempt through reconcile", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "inspect",
      data: {
        state: "failed",
        space: { kind: "id", value: "space-a" },
        required: 12,
        complete: 8,
        missing: 0,
        stale: 0,
        unverified: 0,
        failed_transient: 2,
        failed_permanent: 2,
        orphan: 0,
        backup_supported: true,
        attempt: null,
        backup: null,
        unavailable_reason: null,
        next_action: "resolve_failed",
      },
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: /失敗分を再試行/ }));

    await waitFor(() =>
      expect(startEmbeddingReconcile).toHaveBeenCalledWith({ retry_failed: true }),
    );
    expect(startEmbeddingRebuild).not.toHaveBeenCalled();
  });

  it("does not authorize rebuild operations when a migrating attempt is missing", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "inspect",
      data: {
        state: "migrating",
        space: { kind: "id", value: "space-a" },
        required: 12,
        complete: 8,
        missing: 0,
        stale: 0,
        unverified: 0,
        failed_transient: 2,
        failed_permanent: 2,
        orphan: 0,
        backup_supported: true,
        attempt: null,
        backup: null,
        unavailable_reason: null,
        next_action: "start_rebuild",
      },
    });
    renderPanel();

    expect(await screen.findByRole("alert")).toHaveTextContent(/migrating\/start_rebuild/);
    expect(screen.queryByRole("button", { name: /失敗分を再試行/ })).not.toBeInTheDocument();
    expect(startEmbeddingReconcile).not.toHaveBeenCalled();
  });

  it("does not report a stopped recovery as accepted success", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "inspect",
      data: {
        state: "migrating",
        space: { kind: "id", value: "space-a" },
        required: 10,
        complete: 3,
        missing: 7,
        stale: 0,
        unverified: 0,
        failed_transient: 0,
        failed_permanent: 0,
        orphan: 0,
        backup_supported: true,
        attempt: { attempt: "attempt-1", stage: "replace", backup_mode: "backup" },
        backup: "/backup",
        unavailable_reason: null,
        next_action: "resume_or_restore",
      },
    });
    recoverEmbeddingMigration.mockResolvedValue({
      result: {
        kind: "restored",
        data: {
          outcome: "restored",
          attempt: "attempt-1",
          space: { kind: "id", value: "space-a" },
          next_action: "reconcile",
        },
      },
      restarted: false,
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "バックアップから復元" }));
    fireEvent.click(await screen.findByRole("button", { name: "復元する" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      /サイドカーを再起動できませんでした/,
    );
    expect(screen.queryByText(/操作を受け付けました/)).not.toBeInTheDocument();
  });

  it("rejects a recovery response with the wrong operation kind or attempt", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "inspect",
      data: {
        state: "migrating",
        space: { kind: "id", value: "space-a" },
        required: 10,
        complete: 3,
        missing: 7,
        stale: 0,
        unverified: 0,
        failed_transient: 0,
        failed_permanent: 0,
        orphan: 0,
        backup_supported: true,
        attempt: { attempt: "attempt-1", stage: "replace", backup_mode: "backup" },
        backup: "/backup/attempt-1",
        unavailable_reason: null,
        next_action: "resume_or_restore",
      },
    });
    recoverEmbeddingMigration.mockResolvedValue({
      result: {
        kind: "abandoned",
        data: { outcome: "abandoned", attempt: "attempt-other", next_action: "none" },
      },
      restarted: true,
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "バックアップから復元" }));
    fireEvent.click(screen.getByRole("button", { name: "復元する" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      /応答が要求した操作またはattemptと一致しません/,
    );
    expect(screen.queryByText(/操作を受け付けました/)).not.toBeInTheDocument();
  });

  it("does not treat a non-inspect success as a completed inspection", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "switched",
      data: {
        outcome: "switched",
        attempt: "attempt-1",
        source_space: { kind: "id", value: "source" },
        target_space: "target",
        backup: "/backup",
        next_action: "start_rebuild",
      },
    });
    renderPanel();

    expect(await screen.findByRole("alert")).toHaveTextContent(/unexpected result kind.*switched/i);
    expect(screen.queryByText(/操作を受け付けました/)).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /Embedding 再生成を開始/ }),
    ).not.toBeInTheDocument();
  });

  it("does not infer zero failed work from incomplete CLI verification counts", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "failed",
      data: {
        command: "inspect_embedding_migration",
        stage: "inspect",
        error_code: "finalize_required",
        resolution: "finalize_required",
        attempt: { attempt: "attempt-1", stage: "commit", backup_mode: "backup" },
        backup: "/backup/attempt-1",
        cancel_operation: null,
        verification_counts: {
          failed_memory_text: 0,
          failed_memory_media: 0,
          failed_thread: 0,
          // failed_reflection_intent is required by the CLI contract.
        },
      },
    });
    renderPanel();

    expect(await screen.findByText(/状態応答が不正です/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /失敗を受け入れて確定/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "移行を確定" })).not.toBeInTheDocument();
  });

  it.each([-1, 1.5])(
    "rejects non-integer or negative CLI failure counts (%s)",
    async (invalidCount) => {
      inspectEmbeddingMigration.mockResolvedValue({
        kind: "failed",
        data: {
          command: "inspect_embedding_migration",
          stage: "inspect",
          error_code: "finalize_required",
          resolution: "finalize_required",
          attempt: { attempt: "attempt-1", stage: "commit", backup_mode: "backup" },
          backup: "/backup/attempt-1",
          cancel_operation: null,
          verification_counts: {
            missing_memory_text: 0,
            stale_memory_text: 0,
            unverified_memory_text: 0,
            failed_memory_text: 0,
            missing_memory_media: 0,
            stale_memory_media: 0,
            unverified_memory_media: 0,
            failed_memory_media: 0,
            missing_thread: 0,
            stale_thread: 0,
            unverified_thread: 0,
            failed_thread: invalidCount,
            missing_reflection_intent: 0,
            stale_reflection_intent: 0,
            unverified_reflection_intent: 0,
            failed_reflection_intent: 0,
            orphan: 0,
          },
        },
      });
      renderPanel();

      expect(await screen.findByText(/complete verification failure counts/)).toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "移行を確定" })).not.toBeInTheDocument();
      expect(
        screen.queryByRole("button", { name: /失敗を受け入れて確定/ }),
      ).not.toBeInTheDocument();
    },
  );

  it("rejects invalid values in the rest of the verification counts", async () => {
    inspectEmbeddingMigration.mockResolvedValue({
      kind: "failed",
      data: {
        command: "inspect_embedding_migration",
        stage: "inspect",
        error_code: "finalize_required",
        resolution: "finalize_required",
        attempt: { attempt: "attempt-1", stage: "commit", backup_mode: "backup" },
        backup: "/backup/attempt-1",
        cancel_operation: null,
        verification_counts: {
          missing_memory_text: 1.5,
          stale_memory_text: 0,
          unverified_memory_text: 0,
          failed_memory_text: 0,
          missing_memory_media: 0,
          stale_memory_media: 0,
          unverified_memory_media: 0,
          failed_memory_media: 0,
          missing_thread: 0,
          stale_thread: 0,
          unverified_thread: 0,
          failed_thread: 0,
          missing_reflection_intent: 0,
          stale_reflection_intent: 0,
          unverified_reflection_intent: 0,
          failed_reflection_intent: 0,
          orphan: 0,
        },
      },
    });
    renderPanel();

    expect(await screen.findByText(/complete verification failure counts/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "移行を確定" })).not.toBeInTheDocument();
  });

  it("uses finalize failure evidence for an exact second approval without treating unknown inspect counts as zero", async () => {
    inspectEmbeddingMigration.mockResolvedValue(
      inspectResult({
        required: null,
        complete: null,
        missing: null,
        stale: null,
        unverified: null,
        failed_transient: null,
        failed_permanent: null,
        orphan: null,
        attempt: { attempt: "attempt-1", stage: "rebuild_pending", backup_mode: "backup" },
        backup: "/backup",
      }),
    );
    getEmbeddingRebuildProgress.mockResolvedValue({
      attempt_id: "attempt-1",
      status: "complete",
      kinds: [],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: true,
      failed: 3,
      write_rejections: {},
      error: null,
    });
    recoverEmbeddingMigration
      .mockResolvedValueOnce({
        result: finalizeFailure(
          "rebuild_has_failures",
          "resolve_failures",
          "attempt-1",
          verificationCounts({ failed_memory_text: 2, failed_thread: 1 }),
        ),
        restarted: false,
      })
      .mockResolvedValueOnce({
        result: {
          kind: "finalized",
          data: {
            outcome: "finalized",
            attempt: "attempt-1",
            space: "space-a",
            accepted_failed: 3,
            next_action: "apply",
          },
        },
        restarted: true,
      });
    renderPanel();

    const finalize = await screen.findByRole("button", { name: "移行を確定" });
    expect(finalize).toBeEnabled();
    fireEvent.click(finalize);

    await waitFor(() =>
      expect(recoverEmbeddingMigration).toHaveBeenNthCalledWith(1, {
        action: "finalize",
        attempt_id: "attempt-1",
      }),
    );
    expect(await screen.findByText(/検証結果: resolve_failures/)).toBeInTheDocument();
    expect(
      await screen.findByText(/memory_text 2.*memory_media 0.*thread 1.*reflection_intent 0/),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /失敗分を再試行/ })).toBeEnabled();
    expect(
      screen.queryByRole("button", { name: "Embedding 再生成を開始／続行" }),
    ).not.toBeInTheDocument();

    const acceptFailed = await screen.findByRole("button", { name: "失敗を受け入れて確定 (3)" });
    expect(acceptFailed).toBeDisabled();
    fireEvent.change(screen.getByLabelText("失敗件数 3"), { target: { value: "2" } });
    expect(acceptFailed).toBeDisabled();
    fireEvent.change(screen.getByLabelText("失敗件数 3"), { target: { value: "3" } });
    expect(acceptFailed).toBeEnabled();
    fireEvent.click(acceptFailed);

    await waitFor(() =>
      expect(recoverEmbeddingMigration).toHaveBeenNthCalledWith(2, {
        action: "finalize",
        attempt_id: "attempt-1",
        accept_failed: 3,
      }),
    );
  });

  it("retries only failed work from same-attempt rebuild_has_failures evidence and clears it after success", async () => {
    inspectEmbeddingMigration.mockResolvedValue(
      inspectResult({
        required: null,
        complete: null,
        missing: null,
        stale: null,
        unverified: null,
        failed_transient: null,
        failed_permanent: null,
        orphan: null,
        attempt: {
          attempt: "attempt-retry-failures",
          stage: "rebuild_pending",
          backup_mode: "backup",
        },
      }),
    );
    getEmbeddingRebuildProgress.mockResolvedValue({
      attempt_id: "attempt-retry-failures",
      status: "complete",
      kinds: [],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: true,
      failed: 2,
      write_rejections: {},
      error: null,
    });
    recoverEmbeddingMigration.mockResolvedValueOnce({
      result: finalizeFailure(
        "rebuild_has_failures",
        "resolve_failures",
        "attempt-retry-failures",
        verificationCounts({ failed_memory_text: 2 }),
      ),
      restarted: false,
    });
    startEmbeddingRebuild.mockResolvedValue({
      attempt_id: "attempt-retry-failures",
      task_id: "task-retry-failures",
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "移行を確定" }));
    const retryFailed = await screen.findByRole("button", { name: /失敗分を再試行/ });
    expect(
      screen.queryByRole("button", { name: "Embedding 再生成を開始／続行" }),
    ).not.toBeInTheDocument();
    fireEvent.click(retryFailed);

    await waitFor(() =>
      expect(startEmbeddingRebuild).toHaveBeenCalledWith({
        attempt_id: "attempt-retry-failures",
        retry_failed: true,
      }),
    );
    expect(
      await screen.findByRole("button", { name: "Embedding 再生成を開始／続行" }),
    ).toBeEnabled();
    expect(screen.queryByRole("button", { name: /失敗分を再試行/ })).not.toBeInTheDocument();
  });

  it("offers ordinary rebuild continuation for rebuild_incomplete/resume_rebuild without failed-count approval", async () => {
    inspectEmbeddingMigration.mockResolvedValue(
      inspectResult({
        required: null,
        complete: null,
        missing: null,
        stale: null,
        unverified: null,
        failed_transient: null,
        failed_permanent: null,
        orphan: null,
        attempt: { attempt: "attempt-incomplete", stage: "rebuild_pending", backup_mode: "backup" },
      }),
    );
    getEmbeddingRebuildProgress.mockResolvedValue({
      attempt_id: "attempt-incomplete",
      status: "complete",
      kinds: [],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: true,
      failed: 0,
      write_rejections: {},
      error: null,
    });
    recoverEmbeddingMigration.mockResolvedValue({
      result: finalizeFailure(
        "rebuild_incomplete",
        "resume_rebuild",
        "attempt-incomplete",
        verificationCounts({ missing_memory_text: 1 }),
      ),
      restarted: false,
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "移行を確定" }));

    const resume = await screen.findByRole("button", {
      name: "Embedding 再生成を開始／続行",
    });
    expect(screen.queryByRole("button", { name: /失敗分を再試行/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /失敗を受け入れて確定/ })).not.toBeInTheDocument();
    fireEvent.click(resume);

    await waitFor(() =>
      expect(startEmbeddingRebuild).toHaveBeenCalledWith({ attempt_id: "attempt-incomplete" }),
    );
  });

  it("does not reuse last CLI failure evidence after inspection confirms a new attempt", async () => {
    inspectEmbeddingMigration.mockResolvedValue(
      inspectResult({
        required: null,
        complete: null,
        missing: null,
        stale: null,
        unverified: null,
        failed_transient: null,
        failed_permanent: null,
        orphan: null,
        attempt: { attempt: "attempt-before-new", stage: "rebuild_pending", backup_mode: "backup" },
      }),
    );
    getEmbeddingRebuildProgress.mockResolvedValue({
      attempt_id: "attempt-before-new",
      status: "complete",
      kinds: [],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: true,
      failed: 1,
      write_rejections: {},
      error: null,
    });
    recoverEmbeddingMigration.mockResolvedValue({
      result: finalizeFailure(
        "rebuild_has_failures",
        "resolve_failures",
        "attempt-before-new",
        verificationCounts({ failed_memory_text: 1 }),
      ),
      restarted: false,
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "移行を確定" }));
    expect(await screen.findByRole("button", { name: /失敗分を再試行/ })).toBeInTheDocument();

    inspectEmbeddingMigration.mockResolvedValue(
      inspectResult({
        required: null,
        complete: null,
        missing: null,
        stale: null,
        unverified: null,
        failed_transient: null,
        failed_permanent: null,
        orphan: null,
        attempt: { attempt: "attempt-after-new", stage: "rebuild_pending", backup_mode: "backup" },
      }),
    );
    fireEvent.click(screen.getByRole("button", { name: "状態を再確認" }));

    expect(
      await screen.findByRole("button", { name: "Embedding 再生成を開始／続行" }),
    ).toBeEnabled();
    expect(screen.queryByRole("button", { name: /失敗分を再試行/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /失敗を受け入れて確定/ })).not.toBeInTheDocument();
  });

  it("rejects rebuild_incomplete/resolve_failures without retry or failure approval controls", async () => {
    inspectEmbeddingMigration.mockResolvedValue(
      inspectResult({
        required: null,
        complete: null,
        missing: null,
        stale: null,
        unverified: null,
        failed_transient: null,
        failed_permanent: null,
        orphan: null,
        attempt: {
          attempt: "attempt-invalid-pair",
          stage: "rebuild_pending",
          backup_mode: "backup",
        },
      }),
    );
    getEmbeddingRebuildProgress.mockResolvedValue({
      attempt_id: "attempt-invalid-pair",
      status: "complete",
      kinds: [],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: true,
      failed: 1,
      write_rejections: {},
      error: null,
    });
    recoverEmbeddingMigration.mockResolvedValue({
      result: finalizeFailure(
        "rebuild_incomplete",
        "resolve_failures",
        "attempt-invalid-pair",
        verificationCounts({ failed_memory_text: 1 }),
      ),
      restarted: false,
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "移行を確定" }));

    await waitFor(() =>
      expect(
        screen
          .getAllByRole("alert")
          .some((alert) =>
            /inconsistent rebuild failure code\/resolution pair/.test(alert.textContent ?? ""),
          ),
      ).toBe(true),
    );
    expect(screen.queryByRole("button", { name: /失敗分を再試行/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /失敗を受け入れて確定/ })).not.toBeInTheDocument();
  });

  it("does not cache a rebuild_has_failures result for a different attempt", async () => {
    inspectEmbeddingMigration.mockResolvedValue(
      inspectResult({
        required: null,
        complete: null,
        missing: null,
        stale: null,
        unverified: null,
        failed_transient: null,
        failed_permanent: null,
        orphan: null,
        attempt: { attempt: "attempt-current", stage: "rebuild_pending", backup_mode: "backup" },
      }),
    );
    getEmbeddingRebuildProgress.mockResolvedValue({
      attempt_id: "attempt-current",
      status: "complete",
      kinds: [],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: true,
      failed: 2,
      write_rejections: {},
      error: null,
    });
    recoverEmbeddingMigration.mockResolvedValue({
      result: finalizeFailure(
        "rebuild_has_failures",
        "resolve_failures",
        "attempt-other",
        verificationCounts({ failed_memory_text: 2 }),
      ),
      restarted: false,
    });
    renderPanel();

    fireEvent.click(await screen.findByRole("button", { name: "移行を確定" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(/失敗/);
    expect(screen.queryByRole("button", { name: /失敗分を再試行/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /失敗を受け入れて確定/ })).not.toBeInTheDocument();
  });
});

describe("embeddingMigrationInspectRefetchMs", () => {
  // Each inspection spawns the migration CLI, so it must only poll while the
  // state can change without a user action.
  it("polls while an attempt or migration is in flight", () => {
    expect(embeddingMigrationInspectRefetchMs(inspectResult())).toBe(5000);
    expect(
      embeddingMigrationInspectRefetchMs(
        inspectResult({ state: "consistent", attempt: null, next_action: "wait" }),
      ),
    ).toBe(5000);
    expect(
      embeddingMigrationInspectRefetchMs(inspectResult({ state: "migrating", attempt: null })),
    ).toBe(5000);
  });

  it("keeps polling while there is no usable observation", () => {
    expect(embeddingMigrationInspectRefetchMs(undefined)).toBe(5000);
    expect(
      embeddingMigrationInspectRefetchMs(inspectResult({ state: "unavailable", attempt: null })),
    ).toBe(5000);
  });

  it("stops polling once the migration state is settled", () => {
    expect(
      embeddingMigrationInspectRefetchMs(
        inspectResult({ state: "consistent", attempt: null, next_action: "none" }),
      ),
    ).toBe(false);
    expect(
      embeddingMigrationInspectRefetchMs(
        inspectResult({ state: "incomplete", attempt: null, next_action: "reconcile" }),
      ),
    ).toBe(false);
  });
});
