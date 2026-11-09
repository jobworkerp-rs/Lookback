import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { LOCAL_CONNECTION_CONFIG } from "@/lib/connectionConfig";
import { remoteConnectionConfig } from "@/test-utils";
import { EmbeddingSpaceStatus } from "./EmbeddingSpaceStatus";

const getConnectionConfig = vi.fn();
const getEmbeddingSpace = vi.fn();
const getEmbeddingRebuildProgress = vi.fn();
const inspectEmbeddingMigration = vi.fn();
const startEmbeddingRebuild = vi.fn();
const startEmbeddingReconcile = vi.fn();
const getEmbeddingReconcileProgress = vi.fn();

vi.mock("@/api", () => ({
  getConnectionConfig: () => getConnectionConfig(),
  getEmbeddingSpace: () => getEmbeddingSpace(),
  getEmbeddingRebuildProgress: (attemptId: string) => getEmbeddingRebuildProgress(attemptId),
  getEmbeddingReconcileProgress: (taskId: string) => getEmbeddingReconcileProgress(taskId),
  inspectEmbeddingMigration: (...args: unknown[]) => inspectEmbeddingMigration(...args),
  startEmbeddingRebuild: (...args: unknown[]) => startEmbeddingRebuild(...args),
  startEmbeddingReconcile: (...args: unknown[]) => startEmbeddingReconcile(...args),
}));

function renderStatus(readyEpoch = 0) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const result = render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <EmbeddingSpaceStatus readyEpoch={readyEpoch} />
      </QueryClientProvider>
    </I18nextProvider>,
  );
  return { ...result, client };
}

describe("EmbeddingSpaceStatus", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    getConnectionConfig.mockReset();
    getEmbeddingSpace.mockReset();
    getEmbeddingRebuildProgress.mockReset();
    inspectEmbeddingMigration.mockReset();
    startEmbeddingRebuild.mockReset();
    startEmbeddingReconcile.mockReset();
    getEmbeddingReconcileProgress.mockReset();
  });

  it("keeps local search available while reporting per-kind rebuild counts and rejection counters", async () => {
    getConnectionConfig.mockResolvedValue(LOCAL_CONNECTION_CONFIG);
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "pending",
      current_space: {
        space_id: "space-a",
        model_id: "model",
        tokenizer_model_id: "model",
        revision: "",
        dimension: 3,
        distance: "cosine",
      },
      mm_embedding_worker_name: "embedding-worker",
      tables: [
        {
          table: "memory",
          record: {
            space: null,
            legacy_accept: true,
            migration_marker: { state: "pending", attempt_id: "attempt-1" },
          },
        },
      ],
      rpc_migration_supported: true,
      write_rejections: { space_mismatch: 2 },
    });
    getEmbeddingRebuildProgress.mockResolvedValue({
      attempt_id: "attempt-1",
      status: "running",
      kinds: [
        {
          kind: "memory_text",
          targets: 10,
          complete: 4,
          missing: 0,
          stale: 0,
          unverified: 0,
          failed_transient: 1,
          failed_permanent: 0,
          dispatched: 6,
          failed_to_dispatch: 0,
        },
      ],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: false,
      failed: 1,
      write_rejections: { space_mismatch: 2 },
      error: null,
    });
    renderStatus();

    expect(await screen.findByText(/4.*10/)).toBeInTheDocument();
    expect(screen.getByText(/memory_text/)).toBeInTheDocument();
    expect(
      screen.getByText(
        "Embedding の書込みを拒否しました: 別の Embedding 空間（モデル・次元）の結果（2 件）。設定を確認してください。",
      ),
    ).toBeInTheDocument();
    expect(
      screen.getByText(
        /ベクトル検索.*全文検索 \(FTS\).*どちらも検索結果が部分的.*0件でも対象データがないとは限りません/,
      ),
    ).toBeInTheDocument();
    expect(screen.getAllByText(/キーワード検索 \(FTS\) は引き続き利用できます/)).toHaveLength(2);
    expect(screen.getByText(/Embedding worker の登録待ち/)).toBeInTheDocument();
  });

  it("does not show a global notice when there is no migration marker and all rejection counts are zero", async () => {
    getConnectionConfig.mockResolvedValue(LOCAL_CONNECTION_CONFIG);
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [
        {
          table: "memory",
          record: {
            space: null,
            legacy_accept: false,
            migration_marker: null,
          },
        },
        {
          table: "thread",
          record: {
            space: null,
            legacy_accept: false,
            migration_marker: null,
          },
        },
      ],
      rpc_migration_supported: true,
      write_rejections: {
        dimension: 0,
        source_version: 0,
        attempt: 0,
        space: 0,
        missing_token: 0,
      },
    });
    const { client } = renderStatus();

    await waitFor(() =>
      expect(client.getQueryData(["embedding-space", "local", 0])).toEqual(
        expect.objectContaining({ registration_state: "registered" }),
      ),
    );
    expect(screen.queryByText(/Embedding migration marker はありません/)).not.toBeInTheDocument();
    expect(screen.queryByRole("status", { name: "Embedding 状態" })).not.toBeInTheDocument();
    expect(screen.queryByText(/書込み拒否/)).not.toBeInTheDocument();
  });

  it("does not show a global notice when only stale results were discarded", async () => {
    getConnectionConfig.mockResolvedValue(LOCAL_CONNECTION_CONFIG);
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [],
      rpc_migration_supported: true,
      write_rejections: { source_version_mismatch: 1, attempt_mismatch: 2 },
    });
    const { client } = renderStatus();

    await waitFor(() =>
      expect(client.getQueryData(["embedding-space", "local", 0])).toEqual(
        expect.objectContaining({ write_rejections: expect.any(Object) }),
      ),
    );
    expect(screen.queryByRole("status", { name: "Embedding 状態" })).not.toBeInTheDocument();
  });

  it("reports only rejections that need attention in the global notice", async () => {
    getConnectionConfig.mockResolvedValue(LOCAL_CONNECTION_CONFIG);
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [],
      rpc_migration_supported: true,
      write_rejections: { space_mismatch: 2, source_version_mismatch: 1 },
    });
    renderStatus();

    expect(
      await screen.findByText(
        "Embedding の書込みを拒否しました: 別の Embedding 空間（モデル・次元）の結果（2 件）。設定を確認してください。",
      ),
    ).toBeInTheDocument();
    expect(screen.queryByText(/元データの更新後に届いた古い結果/)).not.toBeInTheDocument();
    expect(screen.queryByText(/source_version_mismatch/)).not.toBeInTheDocument();
  });

  it("does not show a global migration notice for a pending worker without a marker", async () => {
    getConnectionConfig.mockResolvedValue(LOCAL_CONNECTION_CONFIG);
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "pending",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [
        {
          table: "memory",
          record: {
            space: null,
            legacy_accept: false,
            migration_marker: null,
          },
        },
      ],
      rpc_migration_supported: true,
      write_rejections: { dimension: 0, source_version: 0, attempt: 0, space: 0, missing_token: 0 },
    });
    const { client } = renderStatus();

    await waitFor(() =>
      expect(client.getQueryData(["embedding-space", "local", 0])).toEqual(
        expect.objectContaining({ registration_state: "pending" }),
      ),
    );
    expect(screen.queryByRole("status", { name: "Embedding 状態" })).not.toBeInTheDocument();
  });

  it("keeps a dismissed migration notice hidden for the same attempt and shows a new attempt", async () => {
    getConnectionConfig.mockResolvedValue(LOCAL_CONNECTION_CONFIG);
    const spaceWithAttempt = (attemptId: string) => ({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [
        {
          table: "memory",
          record: {
            space: null,
            legacy_accept: false,
            migration_marker: { state: "pending", attempt_id: attemptId },
          },
        },
      ],
      rpc_migration_supported: false,
      write_rejections: {},
    });
    getEmbeddingSpace.mockResolvedValue(spaceWithAttempt("attempt-1"));
    const { client, rerender } = renderStatus();

    expect(await screen.findByText(/attempt-1/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "通知を閉じる" }));
    expect(screen.queryByText(/attempt-1/)).not.toBeInTheDocument();

    await act(async () => {
      await client.invalidateQueries({ queryKey: ["embedding-space", "local", 0] });
    });
    expect(screen.queryByText(/attempt-1/)).not.toBeInTheDocument();

    getEmbeddingSpace.mockResolvedValue(spaceWithAttempt("attempt-2"));
    await act(async () => {
      await client.invalidateQueries({ queryKey: ["embedding-space", "local", 0] });
    });
    expect(await screen.findByText(/attempt-2/)).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "通知を閉じる" }));
    expect(screen.queryByText(/attempt-2/)).not.toBeInTheDocument();
    await act(async () => {
      rerender(
        <I18nextProvider i18n={i18n}>
          <QueryClientProvider client={client}>
            <EmbeddingSpaceStatus readyEpoch={1} />
          </QueryClientProvider>
        </I18nextProvider>,
      );
    });
    expect(await screen.findByText(/attempt-2/)).toBeInTheDocument();
  });

  it("shows a remote migration marker without invoking local CLI management or fake progress", async () => {
    getConnectionConfig.mockResolvedValue(remoteConnectionConfig());
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "remote-worker",
      tables: [
        {
          table: "thread",
          record: {
            space: null,
            legacy_accept: false,
            migration_marker: { state: "pending", attempt_id: "remote-attempt" },
          },
        },
      ],
      rpc_migration_supported: false,
      write_rejections: {},
    });
    renderStatus();

    expect(await screen.findAllByText(/Remote server/)).toHaveLength(2);
    expect(await screen.findByText(/進捗率はこのアプリから取得できません/)).toBeInTheDocument();
    expect(
      screen.getByText(
        /ベクトル検索.*全文検索 \(FTS\).*どちらも検索結果が部分的.*0件でも対象データがないとは限りません/,
      ),
    ).toBeInTheDocument();
    expect(screen.getAllByText(/キーワード検索 \(FTS\) は引き続き利用できます/)).toHaveLength(1);
    expect(getEmbeddingRebuildProgress).not.toHaveBeenCalled();
    expect(inspectEmbeddingMigration).not.toHaveBeenCalled();
    expect(startEmbeddingRebuild).not.toHaveBeenCalled();
    expect(startEmbeddingReconcile).not.toHaveBeenCalled();
  });

  it("retains the last known marker and displays a warning after a refresh error", async () => {
    getConnectionConfig.mockResolvedValue(LOCAL_CONNECTION_CONFIG);
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [
        {
          table: "memory",
          record: {
            space: null,
            legacy_accept: false,
            migration_marker: { state: "pending", attempt_id: "attempt-last" },
          },
        },
      ],
      rpc_migration_supported: false,
      write_rejections: {},
    });
    const { client } = renderStatus();

    expect(await screen.findByText(/attempt-last/)).toBeInTheDocument();
    getEmbeddingSpace.mockRejectedValueOnce(new Error("space refresh unavailable"));
    await act(async () => {
      await client.invalidateQueries({ queryKey: ["embedding-space", "local", 0] });
    });

    expect(screen.getByText(/attempt-last/)).toBeInTheDocument();
    expect(await screen.findByText(/space refresh unavailable/)).toBeInTheDocument();
  });

  it("keeps the last known status visible when connection refresh fails", async () => {
    getConnectionConfig.mockResolvedValue(LOCAL_CONNECTION_CONFIG);
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [
        {
          table: "memory",
          record: {
            space: null,
            legacy_accept: false,
            migration_marker: { state: "pending", attempt_id: "attempt-before-error" },
          },
        },
      ],
      rpc_migration_supported: false,
      write_rejections: {},
    });
    const { client } = renderStatus();

    expect(await screen.findByText(/attempt-before-error/)).toBeInTheDocument();
    getConnectionConfig.mockRejectedValueOnce(new Error("connection refresh unavailable"));
    await act(async () => {
      await client.invalidateQueries({ queryKey: ["connection-config"] });
    });

    expect(screen.getByText(/attempt-before-error/)).toBeInTheDocument();
    expect(await screen.findByText(/connection refresh unavailable/)).toBeInTheDocument();
  });

  it("does not display progress reported for a different pending attempt", async () => {
    getConnectionConfig.mockResolvedValue(LOCAL_CONNECTION_CONFIG);
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [
        {
          table: "memory",
          record: {
            space: null,
            legacy_accept: false,
            migration_marker: { state: "pending", attempt_id: "attempt-current" },
          },
        },
      ],
      rpc_migration_supported: true,
      write_rejections: {},
    });
    getEmbeddingRebuildProgress.mockResolvedValue({
      attempt_id: "attempt-stale",
      status: "complete",
      kinds: [
        {
          kind: "memory_text",
          targets: 99,
          complete: 99,
          missing: 0,
          stale: 0,
          unverified: 0,
          failed_transient: 0,
          failed_permanent: 0,
          dispatched: 99,
          failed_to_dispatch: 0,
        },
      ],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: true,
      failed: 0,
      write_rejections: {},
      error: null,
    });
    renderStatus();

    expect(await screen.findByText(/attempt-current/)).toBeInTheDocument();
    expect(await screen.findByText(/別の移行attemptの進捗/)).toBeInTheDocument();
    expect(screen.queryByText(/99/)).not.toBeInTheDocument();
    expect(screen.queryByText(/再生成の完了通知を待っています/)).not.toBeInTheDocument();
  });

  it("refetches embedding markers and progress when the ready epoch changes", async () => {
    getConnectionConfig.mockResolvedValue(LOCAL_CONNECTION_CONFIG);
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [
        {
          table: "memory",
          record: {
            space: null,
            legacy_accept: false,
            migration_marker: { state: "pending", attempt_id: "attempt-epoch" },
          },
        },
      ],
      rpc_migration_supported: true,
      write_rejections: {},
    });
    const progress = (complete: number) => ({
      attempt_id: "attempt-epoch",
      status: "running",
      kinds: [
        {
          kind: "memory_text",
          targets: 10,
          complete,
          missing: 10 - complete,
          stale: 0,
          unverified: 0,
          failed_transient: 0,
          failed_permanent: 0,
          dispatched: complete,
          failed_to_dispatch: 0,
        },
      ],
      orphan: 0,
      caption_missing: 0,
      captions_requested: 0,
      completion_expected: false,
      failed: 0,
      write_rejections: {},
      error: null,
    });
    getEmbeddingRebuildProgress
      .mockResolvedValueOnce(progress(2))
      .mockResolvedValueOnce(progress(8));
    const { rerender, client } = renderStatus(0);

    expect(await screen.findByText(/2.*10/)).toBeInTheDocument();
    await act(async () => {
      rerender(
        <I18nextProvider i18n={i18n}>
          <QueryClientProvider client={client}>
            <EmbeddingSpaceStatus readyEpoch={1} />
          </QueryClientProvider>
        </I18nextProvider>,
      );
    });

    expect(await screen.findByText(/8.*10/)).toBeInTheDocument();
    expect(getEmbeddingRebuildProgress).toHaveBeenCalledTimes(2);
  });
});
